#!/usr/bin/env bash
# Fail if aws-lc-rs (or its sys crates) is anywhere in the dependency tree (ADR-0037: ring is the
# only crypto provider).
#
# Usage: scripts/check-no-aws-lc.sh
#
# Equivalent to `cargo tree -i aws-lc-rs` coming back empty, but that command errors out when the
# package is absent, so list the whole tree instead. All targets and all edge kinds (normal, build
# and dev, as `cargo tree -i` does by default) count: a test-only aws-lc-rs still needs its C/cmake
# build in CI. Used by CI (.github/workflows/ci.yml) and the release workflow.
set -euo pipefail

tree=$(cargo tree --locked --target all --prefix none --format '{p}')
if found=$(grep -E '^aws-lc-(rs|sys|fips-sys) ' <<<"$tree"); then
    echo "error: aws-lc-rs is in the dependency tree (ADR-0037):" >&2
    sort -u <<<"$found" >&2
    echo "run 'cargo tree -i <crate> --target all' to see what pulls it in" >&2
    exit 1
fi
echo "ok: no aws-lc-rs in the dependency tree"
