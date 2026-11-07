#!/usr/bin/env bash
# Check that a release binary is statically linked and runs (ADR-0001).
#
# Usage: scripts/check-release-binary.sh <path-to-hoster>
#
# Used by CI (.github/workflows/ci.yml) and the release workflow (.github/workflows/release.yml).
set -euo pipefail

bin=${1:?usage: $0 <path-to-hoster>}
test -x "$bin" || { echo "error: $bin is not an executable file" >&2; exit 1; }

file_out=$(file -b "$bin")
echo "file: $file_out"
# `file` says "static-pie linked" for static PIE binaries (the musl default) and "statically linked"
# for non-PIE static ones; both are fine.
if ! grep -Eq 'static-pie linked|statically linked' <<<"$file_out"; then
    echo "error: $bin is not statically linked" >&2
    exit 1
fi

# glibc's ldd prints "not a dynamic executable" (and exits 1) for static binaries and
# "statically linked" for static-pie ones; anything that lists libraries is a failure.
ldd_out=$(ldd "$bin" 2>&1 || true)
echo "ldd: $ldd_out"
if ! grep -Eq 'not a dynamic executable|statically linked' <<<"$ldd_out"; then
    echo "error: ldd reports shared library dependencies for $bin" >&2
    exit 1
fi

"$bin" --version
echo "size: $(stat -c %s "$bin") bytes"
