# hoster

A single-binary deployment orchestrator for one VPS: rootless podman, an embedded Pingora reverse
proxy with ACME certificates, zero-downtime deploys by image digest, and a small web admin UI.

See [spec.md](spec.md) for what it does and [docs/](docs/) for the architecture and the decision
records.

## Development

The toolchain is pinned in `rust-toolchain.toml`; rustup installs it on first use.

```sh
cargo build                                   # builds the `hoster` binary
cargo test                                    # unit tests (offline; no podman needed)
cargo fmt --check                             # formatting
cargo clippy --all-targets -- -D warnings     # lints
```

CI (`.github/workflows/ci.yml`) runs the same format, lint and test steps on every push and pull
request, and checks that the static musl release build links statically.

## Releases

Pushing a `v*` tag runs `.github/workflows/release.yml`, which tests, builds the static
`x86_64-unknown-linux-musl` binary and publishes it with `SHA256SUMS` and the `deploy/` files as a
GitHub release. See [docs/install.md](docs/install.md).

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
this work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without
any additional terms or conditions.
