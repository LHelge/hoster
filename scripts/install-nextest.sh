#!/usr/bin/env bash
# Install cargo-nextest, the test runner CI and the release workflow use, into ~/.cargo/bin:
# the prebuilt binary of a pinned version, checked against its pinned SHA-256.
#
# Usage: scripts/install-nextest.sh
#
# For CI runners (x86_64 and aarch64 Linux). On a development machine `cargo install
# cargo-nextest --locked` or a package manager does as well; any recent version runs the suite.
# To bump the version, change it here and replace both checksums with the `sha256sum` of
# https://get.nexte.st/<version>/linux and https://get.nexte.st/<version>/linux-arm.
set -euo pipefail

readonly version=0.9.145

case $(uname -m) in
    x86_64)
        platform=linux
        sha256=32aa82416099eb12fffae9cf1a279ad201fecbd3f74826c613e32e9006b29867
        ;;
    aarch64 | arm64)
        platform=linux-arm
        sha256=0ad2815fd91a7ecec3a7e25c749b584f66729ac688fd26b2fd494f7ff94b7fd0
        ;;
    *)
        echo "error: no pinned cargo-nextest for $(uname -m)" >&2
        exit 1
        ;;
esac

bin=${CARGO_HOME:-$HOME/.cargo}/bin
if [[ -x $bin/cargo-nextest ]] && "$bin/cargo-nextest" nextest --version 2>/dev/null | grep -q "^cargo-nextest $version "; then
    echo "ok: cargo-nextest $version is already installed"
    exit 0
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl --proto '=https' --tlsv1.2 -fsSL --retry 3 -o "$tmp/nextest.tar.gz" "https://get.nexte.st/$version/$platform"
echo "$sha256  $tmp/nextest.tar.gz" | sha256sum --check --quiet
mkdir -p "$bin"
tar -xzf "$tmp/nextest.tar.gz" -C "$bin" cargo-nextest
echo "ok: installed cargo-nextest $version"
