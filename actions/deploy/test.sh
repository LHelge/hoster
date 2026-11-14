#!/usr/bin/env bash
# Tests deploy.sh against stub.py, a local stand-in for GitHub's OIDC token endpoint and hoster's
# /hooks/deploy. Needs bash, curl, jq and python3; run from anywhere.
# The expected log lines are single-quoted on purpose: their backticks are literal.
# shellcheck disable=SC2016
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'kill "$stub" 2>/dev/null || true; rm -rf "$work"' EXIT

python3 "$here/stub.py" "$work" &
stub=$!
disown "$stub"
for _ in $(seq 50); do [ -f "$work/port" ] && break; sleep 0.1; done
[ -f "$work/port" ] || { echo "stub.py did not start"; exit 1; }
base="http://127.0.0.1:$(cat "$work/port")"

A=sha256:$(printf '%064d' 0 | tr 0 a)
B=sha256:$(printf '%064d' 0 | tr 0 b)
failures=0

# The polls the stub answers in the next run, in order, the last one for good; reset by run.
polls=()

# run NAME EXPECTED-EXIT [VAR=VALUE…] -- answers…: runs deploy.sh with a GitHub-like environment,
# the stub answering the given lines in order. Leaves out, outputs, summary and requests in $work.
run() {
  local name=$1 expected=$2
  shift 2
  local vars=()
  while [ $# -gt 0 ] && [ "$1" != -- ]; do vars+=("$1"); shift; done
  [ $# -gt 0 ] && shift
  printf '%s\n' "$@" >"$work/answers"
  if [ ${#polls[@]} -gt 0 ]; then printf '%s\n' "${polls[@]}" >"$work/polls"; else : >"$work/polls"; fi
  polls=()
  : >"$work/requests"
  : >"$work/outputs"
  : >"$work/summary"
  current=$name
  local code=0
  env -i PATH="$PATH" HOME="${HOME:-/tmp}" \
    ACTIONS_ID_TOKEN_REQUEST_URL="$base/token?api-version=2.0" \
    ACTIONS_ID_TOKEN_REQUEST_TOKEN=request-token \
    GITHUB_OUTPUT="$work/outputs" GITHUB_STEP_SUMMARY="$work/summary" RUNNER_TEMP="$work" \
    GITHUB_REF_TYPE=tag GITHUB_REF_NAME=v1.4.2 GITHUB_SHA=0123456789abcdef \
    DEPLOY_URL="$base" DEPLOY_STACK=notes DEPLOY_IMAGES="api=$A" DEPLOY_RETRY_DELAY=0 DEPLOY_POLL_INTERVAL=0 \
    ${vars[@]+"${vars[@]}"} bash "$here/deploy.sh" >"$work/out" 2>&1 || code=$?
  if [ "$code" != "$expected" ]; then
    fail_case "exit $code, expected $expected"
  fi
}

fail_case() {
  echo "FAIL $current: $*"
  sed 's/^/    /' "$work/out"
  failures=$((failures + 1))
}

# expect DESCRIPTION JQ-FILTER FILE: the filter must hold for the (slurped) file.
expect() {
  jq -e -s "$2" "$3" >/dev/null || fail_case "$1"
}
expect_output() {
  grep -qxF "$1" "$work/outputs" || fail_case "no output line \`$1\`"
}
expect_log() {
  grep -qF -- "$1" "$work/out" || fail_case "log lacks \`$1\`"
}
deploys() { jq -s "[.[] | select(.kind == \"deploy\")] | $1" "$work/requests"; }
expect_deploys() { [ "$(deploys length)" = "$1" ] || fail_case "$(deploys length) deploy requests, expected $1"; }
polled() { jq -s "[.[] | select(.kind == \"poll\")] | $1" "$work/requests"; }
expect_polls() { [ "$(polled length)" = "$1" ] || fail_case "$(polled length) polls, expected $1"; }
expect_summary() { grep -qF -- "$1" "$work/summary" || fail_case "summary lacks \`$1\`"; }

# A status answer for deployment 8 (ADR-0070): STATE TERMINAL [REASON-JSON].
status() {
  printf '200 {"deployment_id":"8","stack":"notes","state":"%s","terminal":%s,"reason":%s}' "$1" "$2" "${3:-null}"
}
ACCEPTED='202 {"result":"accepted","deployment_id":"8","state":"Received"}'

# Accepted: the body, the token's audience and use, outputs and summary.
run accepted 0 DEPLOY_IMAGES="
  api=$A
  worker = $B
" -- "202 {\"result\":\"accepted\",\"deployment_id\":\"8\",\"state\":\"Received\"}"
expect_deploys 1
expect "body" ".[1].body == {stack: \"notes\", images: {api: \"$A\", worker: \"$B\"}, tag: \"v1.4.2\"}" "$work/requests"
expect "audience is the admin URL" ".[0].kind == \"token\" and .[0].audience == \"$base\"" "$work/requests"
expect "bearer token" '.[1].authorization == "Bearer token-1" and .[1].content_type == "application/json"' "$work/requests"
expect_output "result=accepted"
expect_output "deployment-id=8"
expect_output "deployment-url=$base/stacks/notes/deployments/8"
grep -qF "[8]($base/stacks/notes/deployments/8) accepted (Received)" "$work/summary" ||
  fail_case "summary lacks the deployment link"
grep -qF "| \`worker\` | \`$B\` |" "$work/summary" || fail_case "summary lacks the images"
grep -q 'add-mask::token-1' "$work/out" || fail_case "token not masked"

# A no-op, with the full webhook URL and an explicit audience and tag.
run noop 0 DEPLOY_URL="$base/hooks/deploy/" DEPLOY_AUDIENCE=https://deploy.example DEPLOY_TAG=build-7 \
  -- '200 {"result":"noop","deployment_id":"7"}'
expect "explicit audience" '.[0].audience == "https://deploy.example"' "$work/requests"
expect "explicit tag" '.[1].body.tag == "build-7"' "$work/requests"
expect_output "result=noop"
expect_output "deployment-url=$base/stacks/notes/deployments/7"

# Off a tag, the tag defaults to the short SHA.
run "branch push" 0 GITHUB_REF_TYPE=branch GITHUB_REF_NAME=main \
  -- '202 {"result":"accepted","deployment_id":"9","state":"Queued"}'
expect "short SHA tag" '.[1].body.tag == "0123456"' "$work/requests"

# Refusals fail at once with hoster's reason, without a retry.
run forbidden 1 -- '403 {"error":"not allowed to deploy this stack"}'
expect_deploys 1
expect_log "::error title=hoster deploy::hoster refused the deploy (403): not allowed to deploy this stack."
run unprocessable 1 -- '422 {"error":"unknown service `api`"}'
expect_log "(422): unknown service \`api\`"
run unauthorized 1 -- '401 {"error":"invalid token"}'
expect_log "hoster refused the token (401): invalid token"

# 503, a dropped connection and 502 are retried, each with a new token.
run retried 0 -- '503 {"error":"cannot verify tokens right now"}' drop \
  '202 {"result":"accepted","deployment_id":"10","state":"Received"}'
expect_deploys 3
[ "$(deploys '[.[].authorization] | unique | length')" = 3 ] || fail_case "a token was reused"
expect_log "::warning title=hoster deploy::attempt 1: 503: cannot verify tokens right now; retrying"
expect_log "::warning title=hoster deploy::attempt 2: no answer; retrying"
expect_output "deployment-id=10"
run "retries exhausted" 1 -- '502 {"error":"registry unreachable"}' '502 {"error":"registry unreachable"}' \
  '502 {"error":"registry unreachable"}'
expect_deploys 3
expect_log "did not take the deploy after 3 attempts (last: 502: registry unreachable)"

# Without `wait`, nothing is polled; the state output is the 202's.
run "no wait" 0 -- "$ACCEPTED"
expect_polls 0
expect_output "state=Received"

# `wait`: polled with the deploy's own token until it is terminal.
polls=("$(status Pulling false)" "$(status HealthChecking false)" "$(status Live true)")
run "wait, live" 0 DEPLOY_WAIT=true -- "$ACCEPTED"
expect_polls 3
expect "polls the deployment" '[.[] | select(.kind == "poll") | .path] | unique == ["/hooks/deploy/notes/deployments/8"]' "$work/requests"
[ "$(polled '[.[].authorization] | unique')" = '[
  "Bearer token-1"
]' ] || fail_case "polls did not reuse the deploy's token"
expect_log "deployment 8: Pulling"
expect_log "deployment 8: HealthChecking"
expect_log "deployment 8 is live: $base/stacks/notes/deployments/8"
expect_output "state=Live"
expect_output "deployment-url=$base/stacks/notes/deployments/8"
expect_summary "deployment [8]($base/stacks/notes/deployments/8) is live"

polls=("$(status LiveWithWarnings true '"cleanup: old container still stopping"')")
run "wait, live with warnings" 0 DEPLOY_WAIT=true -- "$ACCEPTED"
expect_log "::warning title=hoster deploy::deployment 8 went live with warnings: cleanup: old container still stopping"
expect_output "state=LiveWithWarnings"

polls=("$(status HealthChecking false)" "$(status Failed true '"api: health check failed after 30s"')")
run "wait, failed" 1 DEPLOY_WAIT=true -- "$ACCEPTED"
expect_log "::error title=hoster deploy::deployment 8 ended Failed: api: health check failed after 30s; see $base/stacks/notes/deployments/8"
expect_output "state=Failed"
expect_output "deployment-id=8"
expect_summary "deployment [8]($base/stacks/notes/deployments/8) ended **Failed**: api: health check failed after 30s"

polls=("$(status Superseded true)")
run "wait, superseded" 1 DEPLOY_WAIT=true -- "$ACCEPTED"
expect_log "deployment 8 ended Superseded"
expect_output "state=Superseded"

polls=("$(status HealthChecking false)")
run "wait, timeout" 1 DEPLOY_WAIT=true DEPLOY_TIMEOUT=1s DEPLOY_POLL_INTERVAL=0.2 -- "$ACCEPTED"
expect_log "deployment 8 did not finish within 1s (last state: HealthChecking); it keeps running in hoster"
expect_output "state=HealthChecking"
expect_summary "did not finish within 1s (HealthChecking)"

# An expired token is replaced; 503 and a dropped connection are retried.
polls=('401 {"error":"invalid token"}' '503 {"error":"cannot verify tokens right now"}' drop "$(status Live true)")
run "wait, re-mint and retry" 0 DEPLOY_WAIT=true -- "$ACCEPTED"
expect_polls 4
[ "$(polled '[.[].authorization]' | jq -c .)" = '["Bearer token-1","Bearer token-2","Bearer token-2","Bearer token-2"]' ] ||
  fail_case "tokens while polling: $(polled '[.[].authorization]' | jq -c .)"
expect_log "following deployment 8: 503; retrying"
expect_log "following deployment 8: no answer; retrying"
expect_output "state=Live"

polls=('401 {"error":"invalid token"}')
run "wait, token refused for good" 1 DEPLOY_WAIT=true -- "$ACCEPTED"
expect_polls 4
expect_log "hoster kept refusing the token (401) while following deployment 8"

# A server without the endpoint, and a deployment the run may not see.
polls=('404 <!doctype html><title>Not found</title>')
run "wait, old server" 1 DEPLOY_WAIT=true -- "$ACCEPTED"
expect_log "hoster has no deployment status endpoint (404)"
expect_summary "accepted, but could not be followed"
polls=('404 {"error":"no such deployment"}')
run "wait, not this run's" 1 DEPLOY_WAIT=true -- "$ACCEPTED"
expect_log "hoster does not show deployment 8 to this workflow run (404)"

# A no-op has nothing to wait for.
run "wait, noop" 0 DEPLOY_WAIT=true -- '200 {"result":"noop","deployment_id":"7"}'
expect_polls 0
expect_output "state=Noop"

# Input errors fail before any request.
run "no images" 1 DEPLOY_IMAGES=" " --
expect_deploys 0
expect_log "images is empty"
run "empty digest" 1 DEPLOY_IMAGES="api=" --
expect_log "no digest for \`api\`"
run "a tag, not a digest" 1 DEPLOY_IMAGES="api=ghcr.io/acme/notes-api:v1" --
expect_log "\`api\` needs a digest"
run "listed twice" 1 DEPLOY_IMAGES="api=$A
api=$B" --
expect_log "\`api\` is listed twice"
run "plain http" 1 DEPLOY_URL=http://hoster.example.com --
expect_log "url must be hoster's https:// admin URL"
run "no id-token permission" 1 ACTIONS_ID_TOKEN_REQUEST_TOKEN= --
expect_log "permissions: id-token: write"
run "bad wait" 1 DEPLOY_WAIT=yes --
expect_log "wait must be \`true\` or \`false\`, got \`yes\`"
run "bad timeout" 1 DEPLOY_TIMEOUT=10min --
expect_log "timeout must be a number of seconds"
run "zero timeout" 1 DEPLOY_TIMEOUT=0m --
expect_log "timeout must be longer than zero"
[ "$(wc -l <"$work/requests" | tr -d ' ')" = 0 ] || { current="no requests on input errors"; fail_case "requests were made"; }

if [ "$failures" -gt 0 ]; then
  echo "$failures failed"
  exit 1
fi
echo "actions/deploy: all cases passed"
