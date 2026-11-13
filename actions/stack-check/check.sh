#!/usr/bin/env bash
# Runs `hoster stack check` from a hoster release on stack files and annotates its findings.
# Run by action.yml, which passes the inputs as environment variables:
#
#   CHECK_FILES          stack files, whitespace-separated; globs are expanded (with **)
#   CHECK_VERSION        a release tag such as v0.8.0, or `latest`
#   CHECK_DENY_WARNINGS  `true` to fail on warnings too
#
# and CHECK_BINARY, which only actions/stack-check/test.sh sets: a hoster binary to run instead of
# downloading one. The downloaded binary is verified against the release's SHA256SUMS.
set -euo pipefail
shopt -s nullglob
shopt -s globstar 2>/dev/null || true   # bash 4+; macOS bash 3.2 runs test.sh too

fail() {
  echo "::error title=hoster stack check::$*"
  exit 1
}

# --- Files --------------------------------------------------------------------------------------

files=()
read -r -d '' -a patterns <<<"${CHECK_FILES:-}" || true   # every line, not only the first
for pattern in ${patterns[@]+"${patterns[@]}"}; do
  case $pattern in
    *[*?[]*)
      # shellcheck disable=SC2206 # the glob is meant to expand
      matches=($pattern)
      [ ${#matches[@]} -gt 0 ] || fail "files: no file matches \`$pattern\`"
      files+=("${matches[@]}")
      ;;
    *) files+=("$pattern") ;;
  esac
done
[ ${#files[@]} -gt 0 ] || fail "files is empty"

args=()
case ${CHECK_DENY_WARNINGS:-false} in
  true) args+=(--deny-warnings) ;;
  false) ;;
  *) fail "deny-warnings must be true or false, got \`$CHECK_DENY_WARNINGS\`" ;;
esac

# --- Binary -------------------------------------------------------------------------------------

hoster=${CHECK_BINARY:-}
if [ -z "$hoster" ]; then
  [ "${RUNNER_OS:-}" = Linux ] || fail "hoster releases are Linux binaries; run this on a Linux runner, not ${RUNNER_OS:-an unknown OS}"
  case ${RUNNER_ARCH:-} in
    X64) target=x86_64-unknown-linux-musl ;;
    ARM64) target=aarch64-unknown-linux-musl ;;
    *) fail "no hoster release for the runner architecture ${RUNNER_ARCH:-(unknown)}" ;;
  esac

  version=${CHECK_VERSION:-latest}
  case $version in
    latest) url=https://github.com/LHelge/hoster/releases/latest/download ;;
    v[0-9]*) url=https://github.com/LHelge/hoster/releases/download/$version ;;
    [0-9]*) url=https://github.com/LHelge/hoster/releases/download/v$version ;;
    *) fail "version must be a release tag such as v0.8.0, or latest; got \`$version\`" ;;
  esac

  dir=$(mktemp -d "${RUNNER_TEMP:-/tmp}/hoster-stack-check.XXXXXX")
  echo "Downloading hoster-$target from $url"
  (
    cd "$dir"
    curl -fsSL --retry 3 -O "$url/hoster-$target" -O "$url/SHA256SUMS" ||
      fail "could not download hoster-$target and SHA256SUMS from $url; is \`$version\` a hoster release (0.4.0 or later)?"
    sha256sum --check --ignore-missing --strict SHA256SUMS >/dev/null ||
      fail "hoster-$target does not match the release's SHA256SUMS"
    chmod +x "hoster-$target"
  )
  hoster=$dir/hoster-$target
  "$hoster" version
fi

# --- Check --------------------------------------------------------------------------------------

# The text report for the log and the exit status; then the same check as JSON for annotations.
status=0
"$hoster" stack check ${args[@]+"${args[@]}"} -- "${files[@]}" || status=$?

report=$("$hoster" stack check --format json -- "${files[@]}" || true)
jq -r '
  # Workflow command escaping: data, then property values.
  def data: gsub("%"; "%25") | gsub("\r"; "%0D") | gsub("\n"; "%0A");
  def prop: data | gsub(":"; "%3A") | gsub(","; "%2C");
  .files[] | .file as $file | .diagnostics[]
  | "::\(.severity) file=\($file | prop)"
    + (if .location then ",line=\(.location.line),col=\(.location.column)" else "" end)
    + ",title=hoster stack check::"
    + ((if .path != "" then "\(.path): " else "" end) + .message | data)
' <<<"$report" 2>/dev/null || echo "::warning title=hoster stack check::could not read the JSON report, so findings are not annotated"

exit "$status"
