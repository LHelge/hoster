#!/usr/bin/env bash
# Deploys image digests to a hoster stack through its deploy webhook (ADR-0042), authenticated
# with a GitHub Actions OIDC token for hoster's audience (ADR-0047). Run by action.yml, which
# passes every input as an environment variable:
#
#   DEPLOY_URL       hoster's admin URL; a trailing `/` or `/hooks/deploy` is dropped
#   DEPLOY_AUDIENCE  the token's `aud`; defaults to DEPLOY_URL without the path
#   DEPLOY_STACK     the stack's name
#   DEPLOY_IMAGES    one `service=digest` per line
#   DEPLOY_TAG       informational; defaults to the tag name on a tag push, else the short SHA
#
# and DEPLOY_RETRY_DELAY (seconds, default 5), which only actions/deploy/test.sh sets.
#
# 200 and 202 succeed; 502, 503 and transport failures are retried with a fresh token (the token
# is single-use, and a repeated deploy of the same digests is a no-op, ADR-0029); anything else
# fails the step with hoster's reason.
set -euo pipefail

fail() {
  echo "::error title=hoster deploy::$*"
  exit 1
}

# --- Inputs -------------------------------------------------------------------------------------

[ -n "${ACTIONS_ID_TOKEN_REQUEST_URL:-}" ] && [ -n "${ACTIONS_ID_TOKEN_REQUEST_TOKEN:-}" ] ||
  fail "no OIDC token can be requested: give the job \`permissions: id-token: write\` (pull requests from forks never get one)"

base=${DEPLOY_URL:-}
base=${base%/}
base=${base%/hooks/deploy}
base=${base%/}
case $base in
  https://?*) ;;
  # Plain HTTP would send the token in the clear; only a local test server may use it.
  http://127.0.0.1:* | http://localhost:*) ;;
  *) fail "url must be hoster's https:// admin URL, got \`${DEPLOY_URL:-}\`" ;;
esac
audience=${DEPLOY_AUDIENCE:-$base}

stack=${DEPLOY_STACK:-}
[ -n "$stack" ] || fail "stack is empty"

images='{}'
while IFS= read -r line || [ -n "$line" ]; do
  [[ $line =~ ^[[:space:]]*$ ]] && continue
  if [[ $line =~ ^[[:space:]]*([^=[:space:]]+)[[:space:]]*=[[:space:]]*$ ]]; then
    fail "images: no digest for \`${BASH_REMATCH[1]}\`; does the step id in \`steps.<id>.outputs.digest\` exist?"
  fi
  [[ $line =~ ^[[:space:]]*([^=[:space:]]+)[[:space:]]*=[[:space:]]*([^[:space:]]+)[[:space:]]*$ ]] ||
    fail "images: expected \`service=digest\`, got \`$line\`"
  service=${BASH_REMATCH[1]}
  digest=${BASH_REMATCH[2]}
  [[ $digest =~ ^sha256:[0-9a-f]{64}$ ]] ||
    fail "images: \`$service\` needs a digest (sha256:…), not \`$digest\`; hoster deploys by digest, never by tag"
  jq -e --arg s "$service" 'has($s)' <<<"$images" >/dev/null &&
    fail "images: \`$service\` is listed twice"
  images=$(jq -c --arg s "$service" --arg d "$digest" '. + {($s): $d}' <<<"$images")
done <<<"${DEPLOY_IMAGES:-}"
[ "$images" != '{}' ] || fail "images is empty; give one \`service=digest\` per line"

tag=${DEPLOY_TAG:-}
if [ -z "$tag" ]; then
  if [ "${GITHUB_REF_TYPE:-}" = tag ]; then
    tag=${GITHUB_REF_NAME:-}
  else
    sha=${GITHUB_SHA:-}
    tag=${sha:0:7}
  fi
fi

body=$(jq -nc --arg s "$stack" --argjson i "$images" --arg t "$tag" \
  '{stack: $s, images: $i} + (if $t == "" then {} else {tag: $t} end)')
echo "Deploying to $base: $body"

# --- Request ------------------------------------------------------------------------------------

response=$(mktemp "${RUNNER_TEMP:-/tmp}/hoster-deploy.XXXXXX")
trap 'rm -f "$response"' EXIT
encoded_audience=$(jq -rn --arg a "$audience" '$a | @uri')
attempts=3
delay=${DEPLOY_RETRY_DELAY:-5}

for attempt in $(seq 1 "$attempts"); do
  token=$(curl -fsS --max-time 30 -H "Authorization: bearer $ACTIONS_ID_TOKEN_REQUEST_TOKEN" \
    "$ACTIONS_ID_TOKEN_REQUEST_URL&audience=$encoded_audience" | jq -r '.value // empty') ||
    fail "could not request an OIDC token from GitHub"
  [ -n "$token" ] || fail "GitHub returned no OIDC token"
  echo "::add-mask::$token"

  : >"$response"
  status=$(curl -sS --max-time 60 -o "$response" -w '%{http_code}' -X POST "$base/hooks/deploy" \
    -H 'Content-Type: application/json' \
    -H "Authorization: Bearer $token" \
    --data "$body") || status=000
  reason=$(jq -r '.error // empty' "$response" 2>/dev/null || true)
  [ -n "$reason" ] || reason=$(head -c 500 "$response")

  case $status in
    200 | 202) break ;;
    000 | 502 | 503)
      [ "$attempt" -lt "$attempts" ] ||
        fail "hoster did not take the deploy after $attempts attempts (last: ${status/#000/no answer}${reason:+: $reason})"
      echo "::warning title=hoster deploy::attempt $attempt: ${status/#000/no answer}${reason:+: $reason}; retrying with a new token"
      sleep $((delay * attempt))
      ;;
    401) fail "hoster refused the token (401): $reason. Is \`audience\` hoster's (\`[webhook] audience\` in hoster.toml, else https://<admin_host>)? The exact reason is in hoster's log." ;;
    403) fail "hoster refused the deploy (403): $reason. Check the stack's [deploy] rule (repository_id, refs, environment) against this job; the attempt is listed on the stack's page." ;;
    *) fail "hoster refused the deploy ($status): $reason" ;;
  esac
done

# --- Result -------------------------------------------------------------------------------------

echo "hoster answered $status: $(cat "$response")"
result=$(jq -r '.result // empty' "$response")
id=$(jq -r '.deployment_id // empty' "$response")
state=$(jq -r '.state // empty' "$response")
[ -n "$result" ] && [ -n "$id" ] || fail "unexpected answer from hoster ($status): $(head -c 500 "$response")"
url="$base/stacks/$stack/deployments/$id"

if [ -n "${GITHUB_OUTPUT:-}" ]; then
  {
    echo "result=$result"
    echo "deployment-id=$id"
    echo "deployment-url=$url"
  } >>"$GITHUB_OUTPUT"
fi

if [ "$result" = noop ]; then
  echo "Stack $stack already runs these digests: $url"
  summary="already runs these digests (deployment [$id]($url))"
else
  echo "Stack $stack: deployment $id accepted${state:+ ($state)}: $url"
  summary="deployment [$id]($url) accepted${state:+ ($state)}"
fi
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  {
    echo "### hoster: \`$stack\` $summary"
    echo
    echo "| Service | Digest |"
    echo "|---|---|"
    jq -r 'to_entries[] | "| `\(.key)` | `\(.value)` |"' <<<"$images"
  } >>"$GITHUB_STEP_SUMMARY"
fi
