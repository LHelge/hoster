# ADR-0052: Tag-driven releases, a curlable installer, and self-update from the UI

- Status: Accepted (amends ADR-0023: binary location); install.sh also writes a `Delegate=` drop-in where a controller is missing, restarts the user manager for it only while `hoster.service` is neither enabled nor active and no hoster container runs, and installs catatonit, since ADR-0059
- Date: 2026-11-08

## Context

hoster ships as one static binary (ADR-0001) running as a systemd user service (ADR-0023). Until
now, installing meant following a long manual guide. Upgrading meant replacing a root-owned
`/usr/local/bin/hoster` by hand. The owner wants three things:

- a release for every semver tag on `main`;
- a one-line install;
- updates that can be applied from the admin UI.

The UI requirement decides the layout. The service runs sandboxed (`ProtectSystem=strict`) as the
`hoster` user, so it can only replace a binary it owns.

## Decision

### Releases

- **Trigger.** Pushing a `vX.Y.Z` tag on `main` runs `release.yml`. The existing check that the
  tag matches `version` in `Cargo.toml` stays. `scripts/release.sh X.Y.Z` does four things:
  1. bumps the version;
  2. commits `chore(release): vX.Y.Z`;
  3. tags;
  4. pushes.
- **Targets.** Static musl binaries for `x86_64-unknown-linux-musl` and
  `aarch64-unknown-linux-musl`. aarch64 builds and runs its tests natively on GitHub's ARM
  runner.
- **Notes.** git-cliff generates the release notes from the Conventional Commits since the
  previous tag, grouped by type, using `cliff.toml` in the repository. GitHub's PR-based
  `--generate-notes` is dropped because commits go straight to `main`. There is no
  `CHANGELOG.md`: the GitHub releases are the record.
- **Release files.**
  - `hoster-<target>` for each target;
  - `SHA256SUMS`;
  - `install.sh`;
  - the systemd unit, the sysctl file and the example config.
- **Pre-releases.** A tag with a SemVer pre-release suffix (`v0.2.0-rc.1`) becomes a GitHub
  pre-release. "Latest" always means the newest stable release. A pre-release is installed only
  when its version is named explicitly.

### Binary layout (replaces `/usr/local/bin/hoster` from ADR-0023)

```
~hoster/.local/lib/hoster/
  versions/vX.Y.Z/hoster
  current -> versions/vX.Y.Z
```

- The unit runs `%h/.local/lib/hoster/current/hoster run`.
- `ReadWritePaths` includes `%h/.local/lib/hoster`.
- The current and the previous version are kept; older ones are deleted after a successful
  switch.
- The link is switched atomically: a new symlink is created, then `rename(2)`d over `current`.

### Install script

`curl -fsSL https://github.com/LHelge/hoster/releases/latest/download/install.sh | sudo bash`

- **Versioning.** The script is a release file, so it always matches the binary it installs.
- **Behaviour.** It is idempotent and accepts `--version vX.Y.Z` and `--dry-run`.
- **As root, it:**
  - installs podman with apt or dnf if it is missing;
  - creates the `hoster` user with subuid/subgid ranges;
  - enables linger;
  - installs the sysctl file.
- **As `hoster`, it:**
  - picks the binary for `uname -m`, downloads it and checks it against `SHA256SUMS`;
  - installs it into the layout above;
  - enables `podman.socket` and installs the unit;
  - writes the config skeleton only if no config exists;
  - runs `hoster doctor`.
- **It does not start the service.** The script ends by printing the steps only the owner can
  do: the GitHub handle, the OAuth App and `admin_host`.

### Self-update

- **Checking.** A **System** page in the admin UI shows the running version. "Check for updates"
  queries the GitHub releases API for the latest stable release. hoster makes this outbound call
  only when asked, never in the background. The page shows the new version and its release
  notes, with an **Update** button.
- **CLI.** `hoster update [--version vX.Y.Z]` and `hoster rollback`, run as `hoster`, use the
  same code as the UI.
- **Refusing.** Updating is refused while any deployment is in progress. The button is disabled
  and the CLI exits with an error.
- **Update sequence.**
  1. Download the binary for this architecture.
  2. Verify it against `SHA256SUMS` from the same release.
  3. Install it under `versions/`.
  4. Snapshot the database next to the state directory, reusing the `hoster backup db` code.
  5. Switch `current`.
  6. Shut down gracefully. `Restart=always` starts the new version, which runs its migrations
     on startup.
- **No safeguards.** There is no preflight run of the new binary and no automatic revert. The
  restart has the ingress gap accepted in ADR-0034.

### Reversible migrations and rollback

- **Every migration is reversible.** Each one is a sqlx `<version>_<name>.up.sql` /
  `.down.sql` pair, embedded by `sqlx::migrate!()`. The existing migrations get down scripts
  before v0.1.0; no database exists outside development yet.
- **Tests.** One test fails if any migration lacks a down script. Another runs every up, undoes
  everything, then runs every up again.
- **The newer binary runs the rollback.** An older binary cannot downgrade the schema, because it
  doesn't contain the newer down scripts, and `sqlx` refuses to start on unknown applied
  migrations (`VersionMissing`). `hoster rollback`, run by the current binary:
  1. Asks the previous binary for its schema version (`versions/<prev>/hoster version --json`
     reports its latest migration).
  2. Stops the service.
  3. Snapshots the database.
  4. Runs `Migrator::undo(target)`.
  5. Checks that the database is at `target`. If it is not, it restores the snapshot and aborts.
  6. Switches `current` and starts the service.
- **Manual fallback.** If the current binary cannot run at all, restore the pre-update snapshot
  and repoint `current` by hand. `docs/install.md` documents this.

## Alternatives considered

- **Root-owned binary and a `sudo hoster update` CLI**: keeps the binary out of hoster's reach,
  but there's no one-click update from the UI.
- **Re-running the install script as the only update path**: simplest, but doesn't meet the UI
  requirement.
- **Unattended automatic updates**: a hoster restart, with its ingress gap, could happen at any
  time.
- **minisign signatures or Sigstore attestations**: both protect against a tampered release.
  minisign means managing a key; attestations need `gh` or a sigstore client on the server.
  Rejected for now; either can be added later without changing this design.
- **Preflight self-check and automatic revert via `ExecStartPre`**: rejected as unneeded
  complexity for v1.
- **Waiting for running deploys to finish before updating, or interrupting them**: refusing is
  simpler, and the owner can retry.
- **Downgrading the schema from the older binary, or `ignore_missing`**: the older binary lacks
  the down scripts, and running it against a newer schema is unsafe.
- **Background update checks with a dashboard badge**: rejected. hoster makes no outbound calls
  that the admin didn't trigger.

## Consequences

- **Accepted risk: tampered releases.** Integrity rests on GitHub and TLS. A tampered release
  passes the checksum, which is fetched from the same release.
- **Accepted risk: persistence.** The service user can rewrite its own binary, so a compromise
  of hoster can persist across restarts.
- **A bad release causes downtime** until the admin runs `hoster rollback` over SSH.
- **Rollback loses data** that only the newer schema can hold. That is inherent to down
  migrations.
- **Migration discipline.** A migration without a down script fails CI. A down script must
  restore the previous schema exactly, so that the older binary's own migration checks pass.
- **`docs/install.md` is rewritten** around the script and the new layout.
  `scripts/check-systemd-units.sh` follows the new `ExecStart`.

## Correction (2026-11-08)

`main` is now branch-protected: no direct pushes, linear history, and the CI checks are
required. `scripts/release.sh` therefore cannot commit the version bump and push it to `main`.
Releasing takes two steps:

1. `release.sh prepare X.Y.Z` opens a `release/vX.Y.Z` PR containing the `chore(release)` bump,
   with the git-cliff preview as its body (dropped on 2026-11-09, see below).
2. After that PR is merged, `release.sh tag X.Y.Z` tags the merged commit on `main` and pushes
   only the tag.

The tag is created after merging because GitHub's rebase merge rewrites commit SHAs.

## Correction (2026-11-09)

`release.sh prepare` no longer renders a git-cliff preview into the release PR's body, so cutting a
release needs only `git`, `cargo` and `gh` locally. The PR body says how to finish the release.
The release notes are unchanged: the release workflow still renders them with the pinned git-cliff
and `cliff.toml` when the tag is pushed, and they are the GitHub release's body as before.
