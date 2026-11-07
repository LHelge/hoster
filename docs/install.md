# Installing hoster

hoster is one statically linked binary that runs as a systemd user service (ADR-0001, ADR-0023).
<!-- The host setup (user, linger, sysctl, podman.socket, unit, config, `hoster doctor`) is task
     `meu`; it extends this guide. -->

## Get the binary

Every `v*` tag publishes a GitHub release (`.github/workflows/release.yml`) with these files:

| File | What it is |
| --- | --- |
| `hoster-x86_64-unknown-linux-musl` | The binary: x86_64 Linux, statically linked against musl, so it runs on any distribution with no shared-library dependencies. |
| `SHA256SUMS` | SHA-256 checksums of every file in the release. |
| `hoster.service`, `50-hoster.conf`, example config | The systemd user unit, the sysctl drop-in and an example configuration, copied from `deploy/` in the repository. |

Download the binary and the checksums, verify, and install:

```sh
version=v0.1.0   # the release you want
base=https://github.com/<owner>/hoster/releases/download/$version
curl -fLO "$base/hoster-x86_64-unknown-linux-musl"
curl -fLO "$base/SHA256SUMS"
sha256sum --check --ignore-missing SHA256SUMS
sudo install -m 0755 hoster-x86_64-unknown-linux-musl /usr/local/bin/hoster
hoster --version
```

Upgrading is the same: install the new binary over the old one and restart the service.

## Build it yourself

The release build can be rebuilt from a checkout. It needs the toolchain from
`rust-toolchain.toml` (which includes the `x86_64-unknown-linux-musl` target) and a musl C
compiler for the C parts of dependencies (`musl-tools` on Debian/Ubuntu, `musl` on Arch):

```sh
cargo build --release --locked --target x86_64-unknown-linux-musl
scripts/check-release-binary.sh target/x86_64-unknown-linux-musl/release/hoster
```

The script checks with `file` and `ldd` that the binary is statically linked, as CI does.
