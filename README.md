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
request, checks that `.sqlx/` is up to date, and checks that the static musl release build links
statically.

### State store queries (sqlx)

The state store (`src/store/`) uses sqlx's compile-time checked `query!` / `query_as!` macros.
Builds use the committed offline metadata in `.sqlx/`, so no database is needed to compile. The
schema lives in `migrations/` and is embedded into the binary, which migrates its database at
startup.

After adding or changing a query or a migration, regenerate `.sqlx/` against a scratch database
and commit the result:

```sh
cargo install sqlx-cli --version 0.9.0 --locked --no-default-features --features sqlite
export DATABASE_URL=sqlite:target/sqlx-dev.db
sqlx database create && sqlx migrate run     # after a schema change: rm the file first
cargo sqlx prepare -- --all-targets
```

While `DATABASE_URL` is set the macros check against that database instead of `.sqlx/`; CI runs
`cargo sqlx prepare --check -- --all-targets`.

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
