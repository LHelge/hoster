#!/usr/bin/env bash
# Tests check.sh with a fake hoster binary: file and glob handling, the arguments it passes, the
# annotations it makes from the JSON report and the exit status it keeps. The download is covered
# by CI, which runs the action on site/docs.stack.toml.
# The expected log lines are single-quoted on purpose: their backticks are literal.
# shellcheck disable=SC2016
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/repo/deploy/nested"
touch "$work/repo/deploy/a.stack.toml" "$work/repo/deploy/b.stack.toml" "$work/repo/deploy/nested/c.stack.toml"

# Records its arguments, prints $work/text (exiting with $work/status) or, for --format json,
# $work/json.
cat >"$work/hoster" <<EOF
#!/usr/bin/env bash
printf '%s\n' "\$*" >>"$work/calls"
case " \$* " in
  *" --format json "*) cat "$work/json" ;;
  *) cat "$work/text"; exit "\$(cat "$work/status")" ;;
esac
EOF
chmod +x "$work/hoster"

failures=0
current=

fail_case() {
  echo "FAIL $current: $*"
  sed 's/^/    /' "$work/out"
  failures=$((failures + 1))
}

# run NAME EXPECTED-EXIT [VAR=VALUE…]: runs check.sh in $work/repo.
run() {
  current=$1
  local expected=$2
  shift 2
  : >"$work/calls"
  local code=0
  (cd "$work/repo" && env -i PATH="$PATH" HOME="${HOME:-/tmp}" CHECK_BINARY="$work/hoster" \
    CHECK_FILES=deploy/a.stack.toml "$@" bash "$here/check.sh") >"$work/out" 2>&1 || code=$?
  [ "$code" = "$expected" ] || fail_case "exit $code, expected $expected"
}
expect_log() { grep -qxF -- "$1" "$work/out" || fail_case "no line \`$1\`"; }
expect_call() { grep -qxF -- "$1" "$work/calls" || fail_case "hoster was not called as \`$1\`: $(cat "$work/calls")"; }

# A valid file: the text report is printed, nothing is annotated.
echo 0 >"$work/status"
echo "deploy/a.stack.toml: ok (stack notes, 1 service)" >"$work/text"
echo '{"ok":true,"errors":0,"warnings":0,"files":[{"file":"deploy/a.stack.toml","stack":"notes","services":1,"diagnostics":[]}],"skipped":[]}' >"$work/json"
run valid 0
expect_log "deploy/a.stack.toml: ok (stack notes, 1 service)"
expect_call "stack check -- deploy/a.stack.toml"
expect_call "stack check --format json -- deploy/a.stack.toml"
grep -q '^::' "$work/out" && fail_case "annotated a valid file"

# Errors fail the step and are annotated where they are; the escaping keeps `:` and `,` in data.
echo 1 >"$work/status"
echo "deploy/a.stack.toml:3:8: error: name: invalid" >"$work/text"
cat >"$work/json" <<'EOF'
{"ok":false,"errors":2,"warnings":0,"files":[{"file":"deploy/a.stack.toml","diagnostics":[
  {"severity":"error","path":"name","location":{"line":3,"column":8},"message":"must be lowercase, e.g. `demo`"},
  {"severity":"error","path":"","location":null,"message":"100% wrong:\nsecond line"}]}],"skipped":[]}
EOF
run errors 1
expect_log '::error file=deploy/a.stack.toml,line=3,col=8,title=hoster stack check::name: must be lowercase, e.g. `demo`'
expect_log '::error file=deploy/a.stack.toml,title=hoster stack check::100%25 wrong:%0Asecond line'

# Warnings are annotated as warnings; --deny-warnings is passed on and its status kept.
echo 1 >"$work/status"
echo '{"ok":true,"errors":0,"warnings":1,"files":[{"file":"deploy/a.stack.toml","stack":"notes","services":1,"diagnostics":[{"severity":"warning","path":"services.app.volumes.data","location":{"line":9,"column":8},"message":"unused"}]}],"skipped":[]}' >"$work/json"
run "deny warnings" 1 CHECK_DENY_WARNINGS=true
expect_call "stack check --deny-warnings -- deploy/a.stack.toml"
expect_log '::warning file=deploy/a.stack.toml,line=9,col=8,title=hoster stack check::services.app.volumes.data: unused'

# Globs, ** and several lines of files.
echo 0 >"$work/status"
echo '{"ok":true,"errors":0,"warnings":0,"files":[],"skipped":[]}' >"$work/json"
run globs 0 CHECK_FILES="deploy/*.stack.toml
  other.toml"
expect_call "stack check -- deploy/a.stack.toml deploy/b.stack.toml other.toml"
if shopt -s globstar 2>/dev/null; then
  run globstar 0 CHECK_FILES="deploy/**/*.stack.toml"
  expect_call "stack check -- deploy/a.stack.toml deploy/b.stack.toml deploy/nested/c.stack.toml"
fi

# Input errors fail before hoster runs.
run "no match" 1 CHECK_FILES="stacks/*.toml"
expect_log '::error title=hoster stack check::files: no file matches `stacks/*.toml`'
run "no files" 1 CHECK_FILES=" "
expect_log '::error title=hoster stack check::files is empty'
run "bad deny-warnings" 1 CHECK_DENY_WARNINGS=yes
expect_log '::error title=hoster stack check::deny-warnings must be true or false, got `yes`'
[ -s "$work/calls" ] && { current="input errors"; fail_case "hoster ran"; }

# The download is refused off Linux and for a malformed version, before any request.
run "not linux" 1 CHECK_BINARY= RUNNER_OS=macOS RUNNER_ARCH=ARM64
expect_log '::error title=hoster stack check::hoster releases are Linux binaries; run this on a Linux runner, not macOS'
run "bad version" 1 CHECK_BINARY= RUNNER_OS=Linux RUNNER_ARCH=X64 CHECK_VERSION=main
expect_log '::error title=hoster stack check::version must be a release tag such as v0.8.0, or latest; got `main`'

if [ "$failures" -gt 0 ]; then
  echo "$failures failed"
  exit 1
fi
echo "actions/stack-check: all cases passed"
