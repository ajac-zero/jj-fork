#!/usr/bin/env bash
# Cross-process saved-plan/report contracts. Usage: tests/workflow.sh [path/to/jj-fork]
set -euo pipefail
bin="$(realpath "${1:-target/debug/jj-fork}")"
root="$(mktemp -d /tmp/jj-fork-workflow.XXXXXX)"
trap 'rm -rf "$root"' EXIT
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@example.com GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@example.com

fail() { echo "FAIL: $*" >&2; exit 1; }
equal() { [[ "$1" == "$2" ]] || fail "$3: got '$1', want '$2'"; }
expect() {
  local want="$1"; shift
  set +e; "$@" >"$d/out" 2>"$d/err"; local got=$?; set -e
  if [[ $got != "$want" ]]; then cat "$d/out" "$d/err" >&2; fail "$* exited $got, want $want"; fi
}
rev() { jj --ignore-working-copy log --no-graph -r "bookmarks(exact:\"$1\")" -T commit_id; }
refs() { git for-each-ref --format='%(refname) %(objectname)' refs/heads; }
remote_refs() { git -C "$d/fork.git" for-each-ref --format='%(refname) %(objectname)' refs/heads; }
invoke() { "$bin" --config "$d/config.toml" "$@"; }
fixture() {
  d="$root/$1"; mkdir "$d"
  export JJ_CONFIG="$d/jjconfig.toml"
  printf '[user]\nname = "t"\nemail = "t@example.com"\n' >"$JJ_CONFIG"
  git init -q -b main "$d/upstream-src"
  cd "$d/upstream-src"
  printf 'base\n' >base.txt
  git add .; git commit -qm base
  git clone -q --bare . "$d/upstream.git"
  git clone -q "$d/upstream.git" "$d/fork-src"
  cd "$d/fork-src"; git checkout -qb patch/a
  printf 'one\n' >a.txt; git add .; git commit -qm 'series one'
  printf 'two\n' >a2.txt; git add .; git commit -qm 'series two'
  git checkout -qb fork/main origin/main; git merge -q --no-edit --no-ff patch/a
  git clone -q --bare . "$d/fork.git"
  git clone -q "$d/fork.git" "$d/work"
  cd "$d/work"; git checkout -q fork/main
  cat >"$d/config.toml" <<EOF
[upstream]
url = "$d/upstream.git"
[fork]
mirror_branch = "main"
[checks]
patch = [{ name = "patch", run = '''printf 'patch %s\n' "\$(git rev-parse HEAD)" >> "$d/checks"; test ! -e "$d/fail"''' }]
fork = [{ name = "fork", run = '''printf 'fork %s\n' "\$(git rev-parse HEAD)" >> "$d/checks"; test ! -e "$d/fail"''' }]
EOF
  expect 0 invoke init
  printf 'upstream advance\n' >"$d/upstream-src/new.txt"
  git -C "$d/upstream-src" add .; git -C "$d/upstream-src" commit -qm 'upstream advance'
  git -C "$d/upstream-src" push -q "$d/upstream.git" main
  local_before="$(refs)"; remote_before="$(remote_refs)"
}
save() {
  expect 0 invoke sync --save-plan "$d/plan.json" --report "$d/report.json"
  equal "$(refs)" "$local_before" 'save published maintenance'
  equal "$(remote_refs)" "$remote_before" 'save pushed'
  jq -e '.kind == "jj-fork-plan" and .schema_version == 1 and (.authentication|length)==64 and .payload.outcome=="ready" and (.payload.proposal.mappings|length)==2' "$d/plan.json" >/dev/null
  jq -e '.kind == "jj-fork-report" and .authentication == null and .payload.published_operation == null' "$d/report.json" >/dev/null
}

roundtrip() {
  fixture roundtrip; save
  candidate="$(jq -r '.payload.proposal.checks[]|select(.patch)|.candidate' "$d/plan.json")"
  visible="$(jj --ignore-working-copy log --no-graph -r 'all()' -T 'commit_id ++ "\n"')"
  ! grep -qx "$candidate" <<<"$visible" || fail 'saved candidate visible in source'
  frozen_op="$(jq -r '.payload.frozen.base_operation' "$d/plan.json")"
  equal "$(jj --ignore-working-copy op log --limit 1 --no-graph -T id)" "$frozen_op" 'saved proposal became an operation head'
  count="$(wc -l <"$d/checks")"
  expect 0 "$bin" apply "$d/plan.json" --push --report "$d/applied.json"
  equal "$(rev patch/a)" "$candidate" 'apply recomputed series candidate'
  equal "$(git -C "$d/fork.git" rev-parse patch/a)" "$candidate" 'push did not publish checked candidate'
  fork="$(jq -r '.payload.proposal.checks[]|select(.patch|not)|.candidate' "$d/plan.json")"
  equal "$(rev fork/main)" "$fork" 'apply recomputed fork candidate'
  equal "$(git -C "$d/fork.git" rev-parse fork/main)" "$fork" 'remote fork differs from saved candidate'
  equal "$(wc -l <"$d/checks")" "$((count + 2))" 'historical checks were not rerun'
  jq -e '.payload.published_operation != null and (.payload.push.refs|length)>0 and .payload.exit_code==0' "$d/applied.json" >/dev/null
  equal "$(stat -c %a "$d/plan.json")" 600 'plan permissions'
}

tampering() {
  fixture tampering; save
  count="$(wc -l <"$d/checks")"
  for filter in \
    '.payload.frozen.target = "1111111111111111111111111111111111111111"' \
    '.payload.outcome = "refused"' \
    '.payload.proposal.operation = ("1" * 128)' \
    '.payload.proposal.checks = []' \
    '.payload.proposal.bookmark_changes[0].name = "user/arbitrary"' \
    '.payload.context.config_path = "/must/not/be/read"' \
    '.payload.frozen.target = "not-an-id"' \
    '.schema_version = 999' \
    '.engine_version = "other-engine"' \
    '.kind = "jj-fork-report"' \
    'del(.authentication)' \
    '.payload.unexpected = true'; do
    jq "$filter" "$d/plan.json" >"$d/edited.json"
    expect 1 "$bin" apply "$d/edited.json" --push
    equal "$(refs)" "$local_before" 'edited plan changed source'
    equal "$(remote_refs)" "$remote_before" 'edited plan changed remote'
    equal "$(wc -l <"$d/checks")" "$count" 'edited plan executed checks before authentication'
  done
  { printf '{"schema_version":1,'; tail -c +2 "$d/plan.json"; } >"$d/duplicate.json"
  expect 1 "$bin" apply "$d/duplicate.json"
  expect 1 "$bin" apply "$d/report.json"
}

invalidation() {
  local kind="$1"
  fixture "invalidate-$kind"; save
  case "$kind" in
    op) jj bookmark create --quiet user/concurrent -r 'bookmarks(exact:"patch/a")' ;;
    source) printf 'unsnapshotted user edit\n' >base.txt ;;
    source-untracked) printf 'unsnapshotted new file\n' >new-user-file ;;
    config) printf '\n# changed config file\n' >>"$d/config.toml" ;;
    check-policy) sed -i 's/name = "patch"/name = "different check"/' "$d/config.toml" ;;
    tracking) jj bookmark untrack --quiet patch/a --remote origin ;;
    settings) jj config set --repo snapshot.auto-track 'none()' ;;
    git) git update-ref refs/tags/concurrent "$(git rev-parse refs/heads/patch/a)" ;;
    upstream)
      printf 'next upstream\n' >"$d/upstream-src/next.txt"
      git -C "$d/upstream-src" add .; git -C "$d/upstream-src" commit -qm next
      git -C "$d/upstream-src" push -q "$d/upstream.git" main
      ;;
    upstream-delete) git -C "$d/upstream.git" update-ref -d refs/heads/main ;;
    remote|mirror)
      branch=patch/a; [[ $kind != mirror ]] || branch=main
      git -C "$d/fork-src" checkout -q "$branch"
      printf 'remote writer\n' >"$d/fork-src/remote.txt"
      git -C "$d/fork-src" add .; git -C "$d/fork-src" commit -qm 'concurrent remote change'
      git -C "$d/fork-src" push -q "$d/fork.git" "$branch"
      ;;
  esac
  preserved_local="$(refs)"; preserved_remote="$(remote_refs)"
  expect 20 "$bin" apply "$d/plan.json" --push --report "$d/refused.json"
  equal "$(refs)" "$preserved_local" 'stale plan overwrote local state'
  equal "$(remote_refs)" "$preserved_remote" 'stale plan overwrote remote state'
  if [[ $kind == source ]]; then equal "$(cat base.txt)" 'unsnapshotted user edit' 'stale plan discarded source edit'; fi
  if [[ $kind == source-untracked ]]; then equal "$(cat new-user-file)" 'unsnapshotted new file' 'stale plan discarded new source file'; fi
  if [[ $kind == op ]]; then [[ -n "$(rev user/concurrent)" ]] || fail 'concurrent bookmark lost'; fi
  jq -e '.payload.exit_code == 20 and .payload.published_operation == null' "$d/refused.json" >/dev/null
  if [[ $kind == upstream* ]]; then
    grep -q 'upstream target main@upstream' "$d/out" || fail 'refusal did not identify the upstream target'
  fi
  if [[ $kind == remote || $kind == mirror ]]; then
    grep -q "origin changed since the plan froze ($branch)" "$d/out" || fail 'refusal did not identify the changed fork ref'
  fi
}

unrelated_upstream_refs() {
  fixture upstream-churn
  target="$(git -C "$d/upstream.git" rev-parse main)"
  old="$(git -C "$d/upstream.git" rev-parse main~1)"
  git -C "$d/upstream.git" update-ref refs/heads/pr/move "$old"
  git -C "$d/upstream.git" update-ref refs/heads/pr/delete "$target"
  save
  git -C "$d/upstream.git" update-ref refs/heads/pr/move "$target"
  git -C "$d/upstream.git" update-ref -d refs/heads/pr/delete
  git -C "$d/upstream.git" update-ref refs/heads/pr/new "$old"
  expect 0 "$bin" apply "$d/plan.json" --push
  equal "$(git -C "$d/fork.git" rev-parse main)" "$target" 'unrelated upstream churn blocked the mirror'
  equal "$(git -C "$d/fork.git" rev-parse fork/main)" "$(rev fork/main)" 'fork candidate was not published'
}

custom_upstream_target() {
  local kind="$1"
  fixture "target-$kind"
  target="$(git -C "$d/upstream.git" rev-parse main)"
  old="$(git -C "$d/upstream.git" rev-parse main~1)"
  git -C "$d/upstream.git" update-ref refs/heads/release "$target"
  expression='release@upstream'
  [[ $kind != pinned ]] || expression="$target"
  expect 0 invoke sync --target "$expression" --save-plan "$d/plan.json"
  before="$(refs)"; remote="$(remote_refs)"
  case "$kind" in
    moved) git -C "$d/upstream.git" update-ref refs/heads/release "$old" ;;
    deleted) git -C "$d/upstream.git" update-ref -d refs/heads/release ;;
    unchanged|pinned) git -C "$d/upstream.git" update-ref refs/heads/main "$old" ;;
  esac
  if [[ $kind == moved || $kind == deleted ]]; then
    expect 20 "$bin" apply "$d/plan.json" --push
    equal "$(refs)" "$before" 'custom-target drift published local work'
    equal "$(remote_refs)" "$remote" 'custom-target drift pushed'
    grep -q 'upstream target release@upstream' "$d/out" || fail 'custom target not named in refusal'
  else
    expect 0 "$bin" apply "$d/plan.json" --push
    equal "$(git -C "$d/fork.git" rev-parse main)" "$target" 'custom target was silently changed'
  fi
}

source_mutation_during_apply() {
  fixture apply-source-mutation
  cat >"$d/mutate.sh" <<EOF
#!/usr/bin/env bash
set -euo pipefail
if [[ -e "$d/mutate" ]]; then printf 'check edited source\n' >"$d/work/new-user-file"; fi
EOF
  sed -i "s|test ! -e \"$d/fail\"|bash \"$d/mutate.sh\"|g" "$d/config.toml"
  save
  touch "$d/mutate"
  expect 20 "$bin" apply "$d/plan.json" --push --report "$d/refused.json"
  equal "$(refs)" "$local_before" 'source-mutating recheck published'
  equal "$(remote_refs)" "$remote_before" 'source-mutating recheck pushed'
  equal "$(cat new-user-file)" 'check edited source' 'source mutation was snapshotted/discarded'
  jq -e '.payload.exit_code==20 and .payload.published_operation==null' "$d/refused.json" >/dev/null
}

rerun_failure() {
  fixture rerun; save
  count="$(wc -l <"$d/checks")"; touch "$d/fail"
  expect 20 "$bin" apply "$d/plan.json" --push --report "$d/failed.json"
  equal "$(refs)" "$local_before" 'failed recheck published'
  equal "$(remote_refs)" "$remote_before" 'failed recheck pushed'
  [[ "$(wc -l <"$d/checks")" -gt $count ]] || fail 'historical pass authorized skipping checks'
  jq -e '.payload.plan.checks|any(.outcome=="failed")' "$d/failed.json" >/dev/null
}

not_ready() {
  fixture not-ready; touch "$d/fail"
  expect 20 invoke sync --save-plan "$d/plan.json" --report "$d/report.json"
  equal "$(refs)" "$local_before" 'failure save published'
  jq -e '.payload.outcome == "repair" and (.payload.proposal.mappings|length)==2 and (.payload.proposal.candidates|length)>=2 and (.payload.issues|length)>0' "$d/plan.json" >/dev/null
  rm "$d/fail"
  expect 20 "$bin" apply "$d/plan.json" --push
  equal "$(refs)" "$local_before" 'not-ready plan published a partial plan'
  equal "$(remote_refs)" "$remote_before" 'not-ready plan pushed'
}

expired() {
  fixture expired; save
  operation="$(jq -r '.payload.proposal.operation' "$d/plan.json")"
  # Delete this disposable saved operation, not a published operation. No fallback recompute.
  path=".jj/repo/op_store/operations/$operation"
  [[ -f $path ]] || fail 'operation store layout changed; expiry fixture needs adjustment'
  rm "$path"
  expect 20 "$bin" apply "$d/plan.json" --push
  grep -q 'expired' "$d/out" || fail 'missing expired-plan diagnosis'
  equal "$(refs)" "$local_before" 'expired plan recomputed and published'
}

expired_object() {
  fixture expired-object; save
  candidate="$(jq -r '.payload.proposal.checks[]|select(.patch)|.candidate' "$d/plan.json")"
  object=".git/objects/${candidate:0:2}/${candidate:2}"
  [[ -f $object ]] || fail 'candidate is not loose; expiry fixture needs adjustment'
  rm "$object"
  expect 20 "$bin" apply "$d/plan.json" --push
  grep -q 'expired' "$d/out" || fail 'missing expired-object diagnosis'
  equal "$(refs)" "$local_before" 'missing candidate was recomputed'
  equal "$(remote_refs)" "$remote_before" 'missing candidate was pushed'
}

intermediate_handles() {
  fixture intermediate-handles
  jj new --quiet 'main@upstream' -m 'conflicting first edit'
  printf 'series version\n' >base.txt
  jj new --quiet -m 'revert first edit'
  printf 'base\n' >base.txt
  jj bookmark create --quiet patch/intermediate -r @
  jj new --quiet 'bookmarks(exact:"fork/main")'
  printf 'upstream version\n' >"$d/upstream-src/base.txt"
  git -C "$d/upstream-src" add .; git -C "$d/upstream-src" commit -qm 'conflicting upstream'
  git -C "$d/upstream-src" push -q "$d/upstream.git" main
  before="$(refs)"
  expect 20 invoke sync --save-plan "$d/plan.json"
  equal "$(refs)" "$before" 'intermediate conflict published'
  operation="$(jq -r '.payload.proposal.operation' "$d/plan.json")"
  first="$(jq -r '.payload.proposal.mappings[]|select(.subject=="patch/intermediate")|.copy' "$d/plan.json" | head -1)"
  tip="$(jq -r '.payload.issues[]|select(.id=="series-conflict:patch/intermediate")|.candidate' "$d/plan.json")"
  equal "$(jj --ignore-working-copy --at-op "$operation" log --no-graph -r "$first" -T conflict)" true 'intermediate conflict handle missing'
  equal "$(jj --ignore-working-copy --at-op "$operation" log --no-graph -r "$tip" -T conflict)" false 'fixture tip should be clean'
  jq -e --arg first "$first" --arg tip "$tip" '.payload.proposal.candidates|any(.commit==$first and .conflicted) and any(.commit==$tip and (.conflicted|not))' "$d/plan.json" >/dev/null
  expect 20 "$bin" apply "$d/plan.json"
  equal "$(refs)" "$before" 'clean tip hid intermediate conflict'
}

no_op_guard() {
  fixture no-op
  expect 0 invoke sync --push
  expect 0 invoke assemble --save-plan "$d/plan.json"
  jq -e '.payload.proposal.bookmark_changes|length==0' "$d/plan.json" >/dev/null
  jj bookmark create --quiet user/concurrent -r 'bookmarks(exact:"fork/main")'
  before="$(refs)"; remote="$(remote_refs)"
  expect 20 "$bin" apply "$d/plan.json" --push
  equal "$(refs)" "$before" 'no-op plan bypassed local guard'
  equal "$(remote_refs)" "$remote" 'no-op plan bypassed push guard'
}

bad_output() {
  fixture bad-output
  printf 'preserve me\n' >"$d/protected"
  ln -s "$d/protected" "$d/plan.json"
  expect 1 invoke sync --save-plan "$d/plan.json"
  equal "$(cat "$d/protected")" 'preserve me' 'artifact writer followed symlink'
  equal "$(refs)" "$local_before" 'artifact failure published'
  expect 2 invoke sync --save-plan "$d/other.json" --push
  expect 2 invoke sync --save-plan "$d/other.json" --no-checks
  expect 2 "$bin" apply "$d/plan.json" --no-checks
}

apply_config_override() {
  fixture apply-config; save
  cp "$d/config.toml" "$d/same-config.toml"
  printf '\n# different\n' | cat "$d/config.toml" - >"$d/other-config.toml"
  expect 1 "$bin" --config "$d/other-config.toml" apply "$d/plan.json"
  grep -q 'apply cannot use --config' "$d/err" || fail 'apply config refusal was not explained'
  grep -q 'Omit --config' "$d/err" || fail 'apply config refusal gave no remedy'
  # A different path with identical content is the saved configuration.
  expect 0 "$bin" --config "$d/same-config.toml" apply "$d/plan.json"
}

init_follows_upstream_url() {
  fixture upstream-url
  # init follows the committed upstream URL when the remote points elsewhere (a renamed upstream).
  git remote set-url upstream "$d/old-upstream.git"
  expect 0 invoke init
  equal "$(git remote get-url upstream)" "$(sed -n 's/^url = "\(.*\)"$/\1/p' "$d/config.toml" | head -1)" 'init did not update a stale upstream URL'
  grep -q 'updated remote upstream' "$d/err" || fail 'URL update not reported'
  expect 0 invoke init
  grep -q 'updated remote' "$d/err" && fail 'matching URL was rewritten'
  return 0
}

stored_config_and_skills() {
  fixture stored-config
  cp "$d/config.toml" .jj-fork.toml
  expect 0 "$bin" check --no-checks --no-fetch
  cmp .jj-fork.toml .jj/repo/jj-fork.toml
  # A checkout without the committed file (such as a series) uses the stored copy.
  rm .jj-fork.toml
  expect 0 "$bin" check --no-checks --no-fetch
  (cd / && "$bin" skill >/dev/null) || fail 'skill listing needs a repository'
  expect 0 "$bin" skill
  grep -q setting-up-forks-on-amp "$d/out" || fail 'bundled skills not listed'
  expect 0 "$bin" skill setting-up-forks-on-amp
  grep -q '^name: setting-up-forks-on-amp' "$d/out" || fail 'skill not printed'
  expect 1 "$bin" skill nonexistent
  # A checkout with no file and no stored copy takes the committed file from a ref that has it.
  git add .jj-fork.toml 2>/dev/null || true
  cp "$d/config.toml" .jj-fork.toml
  git add .jj-fork.toml; git commit -qm 'add config'; git push -q origin HEAD:refs/heads/fork/main
  git fetch -q origin; git rm -q --cached .jj-fork.toml; rm -f .jj-fork.toml .jj/repo/jj-fork.toml
  git commit -qm 'drop config'
  expect 0 "$bin" check --no-checks --no-fetch
  grep -q 'using the copy on' "$d/err" || fail 'config was not seeded from a ref'
  cmp "$d/config.toml" .jj/repo/jj-fork.toml
  expect 0 "$bin" skill maintaining-forks-with-jj-fork --install
  cmp .agents/skills/maintaining-forks-with-jj-fork/SKILL.md <("$bin" skill maintaining-forks-with-jj-fork)
  printf 'old\n' >.agents/skills/maintaining-forks-with-jj-fork/SKILL.md
  expect 0 "$bin" check --no-checks --no-fetch
  grep -q 'differs from this jj-fork version' "$d/err" || fail 'stale skill not reported'
}

wrong_repository() {
  fixture wrong-repo; save
  git clone -q "$d/fork.git" "$d/other"
  cd "$d/other"; git checkout -q fork/main
  expect 0 invoke init
  before="$(refs)"
  expect 1 "$bin" apply "$d/plan.json"
  equal "$(refs)" "$before" 'wrong-repo plan published'
}

wrong_workspace() {
  fixture wrong-workspace; save
  jj workspace rename --quiet alternate
  before="$(refs)"
  expect 20 "$bin" apply "$d/plan.json"
  equal "$(refs)" "$before" 'wrong workspace plan published'
}

artifact_paths() {
  fixture inside-unignored
  expect 0 invoke sync --save-plan plan.json --report report.json
  before="$(refs)"; remote="$(remote_refs)"
  expect 20 "$bin" apply plan.json --push
  equal "$(refs)" "$before" 'unignored source artifact bypassed snapshot guard'
  equal "$(remote_refs)" "$remote" 'unignored source artifact pushed'
  [[ -s plan.json && -s report.json ]] || fail 'refusal discarded artifacts'
  fixture inside-ignored
  printf 'plan.json\nreport.json\n' >>.git/info/exclude
  expect 0 invoke sync --save-plan plan.json --report report.json
  expect 0 "$bin" apply plan.json --report report.json
  jq -e '.payload.published_operation != null and .payload.exit_code==0' report.json >/dev/null
}

check_report() {
  fixture check-report
  expect 10 invoke check --report "$d/check.json"
  equal "$(refs)" "$local_before" 'check report moved bookmarks'
  jq -e '.kind=="jj-fork-report" and .payload.exit_code==10 and .payload.published_operation==null and .payload.plan.context.command=="check" and (.payload.plan.checks|any(.kind=="integrity" and .outcome=="passed"))' "$d/check.json" >/dev/null
  expect 1 "$bin" apply "$d/check.json"
  printf '\n[invalid_table]\nunknown = true\n' >>"$d/config.toml"
  expect 1 invoke check --report "$d/invalid-config.json"
  jq -e '.payload.plan==null and .payload.exit_code==1 and (.payload.diagnostics|length)==1' "$d/invalid-config.json" >/dev/null
}

postpublication_failure() {
  fixture postpublish; save
  # Repository-local Git ref lock blocks exporting the successfully published series bookmark.
  # Save/apply proposal CAS is allowed to succeed; no rollback and no push afterward.
  mkdir -p .git/refs/heads/patch
  : >.git/refs/heads/patch/a.lock
  expect 1 "$bin" apply "$d/plan.json" --push --report "$d/partial.json"
  candidate="$(jq -r '.payload.proposal.checks[]|select(.patch)|.candidate' "$d/plan.json")"
  equal "$(rev patch/a)" "$candidate" 'published maintenance rolled back after export failure'
  equal "$(remote_refs)" "$remote_before" 'export failure still pushed'
  jq -e '.payload.published_operation != null and .payload.push == null and .payload.exit_code == 1' "$d/partial.json" >/dev/null
}

passed=0; failed=0
run_case() {
  local name="$1"; shift
  set +e; (set -e; "$@") >"$root/$name.log" 2>&1; local code=$?; set -e
  if [[ $code == 0 ]]; then passed=$((passed+1)); echo "ok: $name";
  else failed=$((failed+1)); echo "not ok: $name"; cat "$root/$name.log"; fi
}
run_case roundtrip roundtrip
run_case authenticated-tampering tampering
for kind in op source source-untracked config check-policy tracking settings git remote mirror upstream upstream-delete; do run_case "invalidate-$kind" invalidation "$kind"; done
run_case unrelated-upstream-refs unrelated_upstream_refs
for kind in unchanged moved deleted pinned; do run_case "custom-upstream-target-$kind" custom_upstream_target "$kind"; done
run_case source-mutating-apply-check source_mutation_during_apply
run_case historical-pass-rerun rerun_failure
run_case not-ready not_ready
run_case expired-operation expired
run_case expired-object expired_object
run_case intermediate-conflict-handles intermediate_handles
run_case saved-no-op-guard no_op_guard
run_case atomic-output-and-flags bad_output
run_case apply-config-override apply_config_override
run_case init-follows-upstream-url init_follows_upstream_url
run_case stored-config-and-skills stored_config_and_skills
run_case wrong-repository wrong_repository
run_case wrong-workspace wrong_workspace
run_case inside-source-artifacts artifact_paths
run_case check-report check_report
run_case postpublication-export-failure postpublication_failure
echo "$passed passed; $failed failed"
[[ $failed == 0 ]]
