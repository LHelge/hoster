# hoster

A single-binary deployment orchestrator for one VPS: rootless podman, an embedded Pingora reverse
proxy with ACME certificates, zero-downtime deploys by image digest, and a small web admin UI
with an event log and mail notifications for failed deploys, crash loops, failing certificates
and their recoveries.

**Documentation: <https://docs.hoster.lhelge.se>**, built from [site/](site/) with
[aphid](https://aphid.lhelge.se). See [spec.md](spec.md) for the specification and [docs/](docs/)
for the architecture and the decision records.

## Quick start

On a Linux VPS (x86_64 or aarch64) with systemd:

```sh
curl -fsSL https://github.com/LHelge/hoster/releases/latest/download/install.sh | sudo bash
```

The script installs podman if needed, creates the `hoster` user, installs the binary and the
systemd user unit, and prints the few steps left to you (your GitHub handle, an OAuth App,
`admin_host` and the ACME contact) before you start the service. It never starts hoster itself,
and re-running it is safe. Add `--dry-run` to see what it would do. Updates come from the admin
UI or `hoster update`. See [Installing hoster](https://docs.hoster.lhelge.se/wiki/install/) for what the script does, updates,
rollback and recovery.

## Development

The toolchain is pinned in `rust-toolchain.toml`; rustup installs it on first use.

```sh
cargo build                                   # builds the `hoster` binary
cargo nextest run                             # tests (offline; no podman needed)
cargo fmt --check                             # formatting
cargo clippy --all-targets -- -D warnings     # lints
```

Tests run with [cargo-nextest](https://nexte.st) (`cargo install cargo-nextest --locked`, or your
package manager; settings in `.config/nextest.toml`). `cargo test` runs the same tests if you
don't have it.

The admin UI's stack editor, `assets/editor.js`, is hoster's own script with no dependencies and
no build step (ADR-0060). Its tokenizer and editing helpers are tested with node's built-in test
runner, with nothing to install from npm. Building and `cargo nextest run` don't need node.

```sh
node --test 'assets/*.test.mjs'               # editor script tests (node 22 or later)
```

CI (`.github/workflows/ci.yml`) runs the same format, lint and test steps on every pull request
and every push to `main`, checks that `.sqlx/` is up to date, and checks that the static musl
release build links statically. On a pull request the static build only runs when the pull
request touches the dependencies, the toolchain, `build.rs` or a workflow; every push to `main`
runs it.

### Tests against real podman

Tests that need rootless podman skip themselves unless `TEST_HOSTER_PODMAN=1` is set
(`HOSTER_PODMAN_SOCKET` overrides the socket path). The main one is the end-to-end zero-downtime
test in `src/e2e_tests.rs` (ADR-0017): it runs hoster's orchestrator and proxy in-process,
deploys a busybox-based app as v1, keeps keep-alive, HTTP/2, new-connection and one streaming
request running through the TLS proxy while it deploys v2 and a broken v3, and fails on any
error or non-2xx answer, on a client that does not switch cleanly from v1 to v2, on a cut
stream, or on v3 ever answering. CI runs it in the `podman` job; locally:

```sh
systemctl --user start podman.socket
TEST_HOSTER_PODMAN=1 cargo nextest run --no-capture e2e_tests   # just the end-to-end test
TEST_HOSTER_PODMAN=1 cargo nextest run                           # every podman test (pulls from ghcr)
```

### State store queries (sqlx)

The state store (`src/store/`) uses sqlx's compile-time checked `query!` / `query_as!` macros.
Builds use the committed offline metadata in `.sqlx/`, so no database is needed to compile. The
schema lives in `migrations/` and is embedded into the binary, which migrates its database at
startup.

Every migration is a reversible `<version>_<name>.up.sql` / `.down.sql` pair, so a newer binary
can roll the schema back for an older one (ADR-0052). Create new ones with
`sqlx migrate add -r <name>`, and make the down script restore the previous schema exactly; the
store tests fail on a migration without a down script or a down script that does not.

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

The log store (`src/store/logs/`, `logs.db`, ADR-0067) is a second database with its own
migrations in `migrations_logs/`; create them with
`sqlx migrate add -r --sequential --source migrations_logs <name>`. The store tests hold them to
the same rules. Its queries are runtime-checked (`sqlx::query`, not `query!`), because the
macros check against one database, `hoster.db`'s, so they need no `.sqlx/` entries and every one
is covered by a test instead.

### Cutting a release

`main` is protected, so a release goes through a PR and the tag is pushed after it is merged
(ADR-0052). `scripts/release.sh` needs `gh` (logged in). The release notes are rendered by the
release workflow when the tag is pushed, so nothing else is needed locally.

Each release also gets a post on the docs site's [Releases](https://docs.hoster.lhelge.se/blog/)
page: `site/content/blog/YYYY-MM-DD_hoster-X-Y-Z.md` with `slug: hoster-X-Y-Z` and
`tags: [release]`, written for users from the commits since the last tag (what changed, and what
to know before updating). The docs site is only deployed by a release tag, so merge the post in its own
`docs:` PR before running `prepare`; the slug is what links the post to its GitHub release.

A RustSec advisory against a shipped dependency (an issue opened by the daily
`.github/workflows/audit.yml`) means a patch release once the fix is in, unless
`.cargo/audit.toml` records why it does not affect hoster.

```sh
git log --oneline "$(git describe --tags --abbrev=0)"..main   # what the release contains
# ... merge the release post (site/content/blog/) through a docs PR ...
scripts/release.sh prepare 0.2.0       # on a clean, up-to-date main: opens the release/v0.2.0 PR
# ... CI passes, rebase-merge the PR on GitHub ...
git switch main && git pull --ff-only
scripts/release.sh tag 0.2.0           # tags the merged chore(release) commit, pushes only the tag
```

`prepare` refuses unless `main` is clean and equal to `origin/main` and the version is valid
SemVer above the one in `Cargo.toml` (the first release, `prepare 0.1.0`, may keep the version
already there as long as it was never tagged; its release commit is empty). It bumps `Cargo.toml`
and `Cargo.lock`, commits `chore(release): v0.2.0`, pushes the branch and opens a PR whose body says
how to finish the release. If it fails before pushing, it switches back to `main` and deletes the local
release branch; if the PR can't be opened after the push, it says how to finish or drop the branch.
`tag` refuses unless local `main` equals `origin/main`, its tip is that commit and `Cargo.toml`
says the version; it asks before pushing (`--yes` skips the prompt). The tag is created only after
merging because GitHub's rebase merge gives the commit a new SHA. A version with a pre-release
suffix (`0.2.0-rc.1`) becomes a GitHub pre-release. `scripts/test-release.sh` exercises both steps
against a local bare remote with `gh` stubbed.

## Releases

Pushing a `v*` tag on `main` runs `.github/workflows/release.yml`. It refuses a tag that doesn't
match `Cargo.toml` or isn't on `main`, tests and builds static musl binaries for
`x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` (natively, on GitHub's ARM runner),
and publishes them with one `SHA256SUMS`, the `deploy/` files and `install.sh` as a GitHub
release. The release notes are git-cliff's grouping of the Conventional Commits since the previous
tag (`cliff.toml`); `chore(release)` commits are left out. See [Installing hoster](https://docs.hoster.lhelge.se/wiki/install/).

The same run builds the documentation site in `site/` with aphid. On a tag it packs the site into
an aphid-server image (`site/Dockerfile`), pushes `ghcr.io/lhelge/hoster-docs` and deploys it to
<https://docs.hoster.lhelge.se> through hoster's own deploy webhook. Run `aphid` in `site/` to
preview it on <http://localhost:3000> with live reload.

App repositories deploy to hoster from their own release workflows with a GitHub Actions OIDC
token; see [Deploying from CI](https://docs.hoster.lhelge.se/wiki/deploying-from-ci/) and the example in
[examples/github-actions/release.yml](examples/github-actions/release.yml).
Static sites are packed into a small static-file server image and deployed the same way; see
[Hosting a static site](https://docs.hoster.lhelge.se/wiki/static-sites/).

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
this work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without
any additional terms or conditions.
