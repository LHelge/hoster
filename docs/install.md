# Installing hoster

hoster is one statically linked binary that runs as a systemd user service of a dedicated
`hoster` user (ADR-0001, ADR-0023). One command sets a host up, and the same command brings it up
to date later (ADR-0052):

```sh
curl -fsSL https://github.com/LHelge/hoster/releases/latest/download/install.sh | sudo bash
```

1. [Install](#install): what you need and the options the script takes.
2. [What the script does](#what-the-script-does), as root and as `hoster`.
3. [Finish the setup](#finish-the-setup): the steps only you can do, then start hoster.
4. [Updates and rollback](#updates-and-rollback), and
   [manual recovery](#manual-recovery) when the current binary cannot run.

[Operating hoster](#operating-hoster) covers logs and stopping, [Accepted risks](#accepted-risks)
what the update design trusts, and [Hardening](#hardening) the sandboxing the unit applies.

## Install

You need a Linux host on x86_64 or aarch64 with systemd as init, cgroup v2 and unprivileged user
namespaces (rootless podman needs them anyway). The script installs podman itself with `apt` or
`dnf` (Debian, Ubuntu, Fedora and relatives); on other distributions install podman, `newuidmap`
/ `newgidmap` and `sysctl` first, and the script uses them.

```sh
# The latest stable release:
curl -fsSL https://github.com/LHelge/hoster/releases/latest/download/install.sh | sudo bash

# A specific release (the only way to get a pre-release), or just print what would happen:
curl -fsSL https://github.com/LHelge/hoster/releases/latest/download/install.sh \
  | sudo bash -s -- --version v0.2.0 --dry-run
```

| Option | Effect |
| --- | --- |
| `--version vX.Y.Z` | Install this release. Without it the script installs the release it was published with, which for `releases/latest/download/install.sh` is the latest stable release. |
| `--dry-run` | Print every command that would change the host instead of running it. The release is still downloaded into a temporary directory and checked, so a bad checksum shows up here too. |
| `-h`, `--help` | Usage. |

The script must run as root. It is idempotent: re-running it on an installed host changes
nothing, unless the chosen release is newer than the installed one. Its last line calls the
function that holds the rest, so a download cut short runs nothing.

To read it before running it, download it, check it against the release's `SHA256SUMS` and run
the file:

```sh
base=https://github.com/LHelge/hoster/releases/latest/download
curl -fLO "$base/install.sh" && curl -fLO "$base/SHA256SUMS"
sha256sum --check --ignore-missing SHA256SUMS
less install.sh
sudo bash install.sh --dry-run
```

## What the script does

**As root**, it:

- refuses to run unless it is root on Linux x86_64 or aarch64 with systemd;
- downloads the sysctl file `50-hoster.conf` and `SHA256SUMS` from the release into a temporary
  directory only root can write and checks the file against `SHA256SUMS`, before it changes
  anything. Root never installs a file the `hoster` user could have written, or writes through a
  path that user controls;
- installs podman (and `uidmap`, `procps`, `dbus-user-session` where missing) with `apt-get` or
  `dnf` if any of `podman`, `newuidmap`, `sysctl`, `runuser` or `useradd` is missing. Without
  either package manager it stops with an error naming the packages to install;
- creates the `hoster` system user with a home directory in `/home/hoster` and a 65536-wide
  subordinate uid and gid range after the highest one already in `/etc/subuid` and
  `/etc/subgid`; an existing user or range is left alone;
- runs `loginctl enable-linger hoster`, so the user's systemd instance (and with it
  `podman.socket` and `hoster.service`) runs at boot without a login;
- installs `50-hoster.conf` as `/etc/sysctl.d/50-hoster.conf`
  (`net.ipv4.ip_unprivileged_port_start = 80`, so hoster can bind :80 and :443) and applies it.

**As `hoster`**, with that user's own systemd instance, it:

- downloads `hoster-<target>` for `uname -m`, the unit, the example config and `SHA256SUMS`
  from the release, and checks every file against `SHA256SUMS`. A missing or wrong sum aborts
  the install before the binary, the unit or the config is installed;
- installs the binary as `~/.local/lib/hoster/versions/vX.Y.Z/hoster` (through a temporary
  file and a rename), checks that it reports the expected version and target
  (`hoster version --json`), and switches `current` to it by renaming a new symlink over the
  old one. Of the older versions it keeps only the newest one, for `hoster rollback`;
- links `~/.local/bin/hoster` to `~/.local/lib/hoster/current/hoster`, unless something else is
  there already;
- enables and starts `podman.socket`, the rootless podman API (ADR-0002);
- installs `~/.config/systemd/user/hoster.service` when it differs and runs
  `systemctl --user daemon-reload`. Your drop-ins in `hoster.service.d/` are never touched;
- creates the state directory `~/.local/share/hoster` and the configuration directory
  `~/.config/hoster` (both mode 0700), and writes the example configuration as
  `~/.config/hoster/hoster.toml` (mode 0600) only if there is none;
- runs `hoster doctor`. On a fresh host it reports failures until the steps below are done.

It **never starts, enables or restarts** `hoster.service`. It ends by printing the remaining
steps.

The binary layout replaces `/usr/local/bin/hoster` (ADR-0052): the service user owns its binary,
so `hoster update` can replace it from inside the sandbox.

```
~hoster/.local/lib/hoster/
  versions/v0.1.0/hoster
  versions/v0.2.0/hoster
  current -> versions/v0.2.0
```

Re-running the script:

| Installed `current` | Chosen release | What happens |
| --- | --- | --- |
| the same version | | Nothing; it reports `nothing to change`. |
| older | newer, not installed yet | Installs and switches to it; prints that a running hoster needs a restart. |
| older | newer, already installed (you rolled back from it) | Keeps `current`, unless `--version` names it. |
| newer | older, the latest stable (you run a pre-release) | Keeps `current`. |
| newer | older, named with `--version` | Refuses: switching back would not downgrade the schema. Use [`hoster rollback`](#updates-and-rollback). |

## Finish the setup

The remaining steps run **as `hoster`**, in a session with that user's own session bus and
`XDG_RUNTIME_DIR`. `sudo -u hoster` and `su hoster` do not set these up; use one of:

```sh
sudo machinectl shell hoster@     # a full session (machinectl is in systemd-container)
ssh hoster@<host>                 # if hoster has an SSH key
```

For a single command, `sudo systemctl --user -M hoster@ <command>` works too.

### The admin's GitHub handle

`HOSTER_ADMIN_GITHUB` names the one GitHub account allowed into the admin UI (ADR-0021). It is
read from the environment only. The shipped unit has an invalid placeholder, and hoster refuses
to start until it is replaced. Put the real value in a drop-in, so that a newer `hoster.service`
does not reset it:

```sh
systemctl --user edit hoster
```

and enter:

```ini
[Service]
Environment=HOSTER_ADMIN_GITHUB=octocat
```

This writes `~/.config/systemd/user/hoster.service.d/override.conf` and reloads systemd. Other
`HOSTER_*` overrides, `RUST_LOG` and a larger `TimeoutStopSec=` go into the same drop-in.

### The GitHub OAuth App

The admin UI logs in with GitHub OAuth (ADR-0021, ADR-0031). Create an OAuth App under GitHub
→ Settings → Developer settings → OAuth Apps, with the callback URL
`https://<admin_host>/auth/github/callback`. hoster requests no scopes; it only reads which
account logged in. Store the client id and a client secret as 0600 files and hand them to hoster
as systemd credentials, in the same drop-in:

```ini
[Service]
LoadCredential=github_client_id:%h/.config/hoster/github_client_id
LoadCredential=github_client_secret:%h/.config/hoster/github_client_secret
```

Alternatively, point `web.github_client_id_file` and `web.github_client_secret_file` in
`hoster.toml` at the files. hoster refuses to start without both. Sessions last 12 hours at most
and end after `web.session_idle_timeout` (default 1h) without a request; the header of every
page has "Log out" and "Log out everywhere".

### The configuration

Edit `~/.config/hoster/hoster.toml` (ADR-0024) and set at least:

- `admin_host`, the reserved host for the admin UI and the deploy webhook, whose DNS points at
  this server;
- `[acme] contact`, the e-mail registered with Let's Encrypt. Use the staging `directory_url`
  for first runs if you expect to retry.

hoster generates its secret-store key as `~/.config/hoster/secret.key` on first start
(ADR-0029); back it up (docs/backup.md).

If you move `state_dir` elsewhere, replace the unit's `ReadWritePaths=` in the drop-in, or
hoster cannot write there. An empty assignment clears the list, and every path listed must exist,
or the unit fails to start (status `226/NAMESPACE`):

```ini
[Service]
ReadWritePaths=
ReadWritePaths=/srv/hoster %h/.config/hoster %h/.local/lib/hoster
```

### Check and start

`hoster doctor` checks the user, the configuration, the state directory, the podman socket, the
sysctl, that :80/:443 can be bound, linger, `podman.socket`, cgroup delegation, the clock and the
binary layout. It reads the same environment as the service, so pass the handle in:

```sh
HOSTER_ADMIN_GITHUB=octocat hoster doctor
```

(`hoster` is `~/.local/bin/hoster`; if that is not on your `PATH`, use
`~/.local/lib/hoster/current/hoster`.) Fix every failed check; each one prints a hint. Then
enable and start the service:

```sh
systemctl --user enable --now hoster
systemctl --user status hoster
```

`enable` adds it to `default.target`, which the lingering user instance reaches at boot.
`Type=notify` means the unit only becomes `active` once hoster reports it is serving.

## Updates and rollback

hoster never checks for updates by itself (ADR-0052). There are three ways to update:

- **The admin UI.** The **System** page shows the running version. "Check for updates" asks the
  GitHub releases API for the latest stable release and shows its version and release notes;
  **Update** installs it.
- **The CLI**, as `hoster`: `hoster update` installs the latest stable release,
  `hoster update --version vX.Y.Z` a specific one (a pre-release only this way). It uses the same
  code as the UI.
- **Re-running `install.sh`**, as above. It installs and switches the binary and updates the
  unit, but neither snapshots the database nor restarts hoster; run
  `systemctl --user restart hoster` afterwards. Prefer `hoster update` on a running host.

`hoster update` (UI or CLI) refuses while a deployment is in progress; retry once it has
finished. Otherwise it downloads the binary for this architecture, checks it against the
release's `SHA256SUMS`, installs it under `versions/`, snapshots the database into the state
directory (the `hoster backup db` code), switches `current` and shuts hoster down gracefully.
`Restart=always` starts the new version, which migrates the database on startup. The restart
has the short ingress gap of ADR-0034. There is no preflight run and no automatic revert.

Run the CLI as hoster, not root (`sudo machinectl shell hoster@` or `sudo -iu hoster`; it finds
the user manager under `/run/user/<uid>` when the session sets no `XDG_RUNTIME_DIR`). Of the
configuration they read only `state_dir`, so unlike `hoster doctor` they need no
`HOSTER_ADMIN_GITHUB`. `hoster update --check` only shows the versions and the release notes; otherwise it asks before
updating (`--yes` skips that). `hoster update` and `hoster rollback` exit 0 when they succeeded
or hoster already is up to date, 1 on an error or a declined confirmation, and 2 on a usage
error (such as no terminal to confirm on without `--yes`).

**Rollback.** If a release misbehaves, go back to the previous version over SSH, as `hoster`:

```sh
hoster rollback
```

It runs in the *current* (newer) binary, because only that one has the down migrations for its
schema: it asks the previous binary for its schema version
(`versions/<previous>/hoster version --json`), stops the service, snapshots the database, undoes
the migrations down to that version, checks the result (restoring the snapshot and stopping if
it is wrong), switches `current` and starts the service. Data that only the newer schema can hold
is lost. A plain re-run of `install.sh` afterwards does not switch forward again; name the
version with `--version` when you want it back.

Running the older binary directly against the newer database does not work: sqlx refuses to
start on migrations it does not know.

## Manual recovery

If the current binary cannot run at all, so `hoster rollback` is not an option, undo the update
by hand, as `hoster`:

```sh
systemctl --user stop hoster
cd ~/.local/lib/hoster
ls versions/                                        # the current and the previous version
ls -lt ~/.local/share/hoster/hoster-pre-update-*.db # the snapshot hoster update wrote

# The database as it was before the update (docs/backup.md, "Restoring"):
rm -f ~/.local/share/hoster/hoster.db-wal ~/.local/share/hoster/hoster.db-shm
install -m 0600 ~/.local/share/hoster/<pre-update snapshot> ~/.local/share/hoster/hoster.db

# Point current at the previous version, atomically:
ln -sfn versions/v0.1.0 current.new && mv -T current.new current

systemctl --user start hoster
```

The snapshot is named `hoster-pre-update-<from>-to-<to>-<unix time>.db` (docs/backup.md).
Changes made after the update are lost with it. If you updated by re-running `install.sh`, there
is no pre-update snapshot: restore your latest backup instead.

## Release files

Every `vX.Y.Z` tag on `main` publishes a GitHub release (`.github/workflows/release.yml`,
ADR-0052) with these files:

| File | What it is |
| --- | --- |
| `hoster-x86_64-unknown-linux-musl`, `hoster-aarch64-unknown-linux-musl` | The binary for x86_64 and aarch64 Linux, statically linked against musl, so it runs on any distribution. |
| `install.sh` | The install script, set to install this release. |
| `hoster.service`, `50-hoster.conf`, `hoster.example.toml` | The systemd user unit, the sysctl drop-in and the example configuration, from `deploy/`. |
| `SHA256SUMS` | SHA-256 checksums of every file above. |

A tag with a pre-release suffix (`v0.2.0-rc.1`) becomes a GitHub pre-release; "latest" always
means the newest stable release.

### Build it yourself

The release build can be rebuilt from a checkout. It needs the toolchain from
`rust-toolchain.toml` (which lists both musl targets) and a musl C compiler for the C parts of
dependencies (`musl-tools` on Debian/Ubuntu, `musl` on Arch):

```sh
cargo build --release --locked --target x86_64-unknown-linux-musl
scripts/check-release-binary.sh target/x86_64-unknown-linux-musl/release/hoster
```

The script checks with `file` and `ldd` that the binary is statically linked, as CI does. To use
your own build, install it into the layout by hand, as `hoster`:

```sh
v=v$(./hoster version --json | sed 's/.*"version":"\([^"]*\)".*/\1/')
install -D -m 0755 hoster ~/.local/lib/hoster/versions/$v/hoster
ln -sfn versions/$v ~/.local/lib/hoster/current.new
mv -T ~/.local/lib/hoster/current.new ~/.local/lib/hoster/current
```

### Testing the install script

`scripts/test-install.sh` runs `install.sh` for real against fake releases served from
`file://` URLs, with a stub binary per version: a dry run on a fresh host, an install, an
idempotent re-run, updates with pruning, a refused downgrade, a checksum mismatch and a
rolled-back version. It changes the host (it creates the `hoster` user), so CI runs it on its
disposable runner and locally it runs in a throwaway systemd container:

```sh
scripts/test-install.sh --container                                  # Debian trixie
scripts/test-install.sh --container docker.io/library/fedora:latest
```

The fake releases are wired in with `HOSTER_INSTALL_RELEASES_URL`, which replaces
`https://github.com/LHelge/hoster/releases` (laid out as `<url>/download/vX.Y.Z/<file>` and
`<url>/latest/download/<file>`). It is meant for testing only. CI also runs `shellcheck` on
`install.sh` and `scripts/`.

## Operating hoster

| Task | Command |
| --- | --- |
| Status | `systemctl --user status hoster` |
| Follow the logs | `journalctl --user -u hoster -f` |
| Logs, as root | `sudo journalctl _SYSTEMD_USER_UNIT=hoster.service` |
| Stop, start, restart | `systemctl --user stop hoster` (`start`, `restart`) |
| Running version | `hoster version`, or the admin UI's System page |
| More log detail | `Environment=RUST_LOG=hoster=debug` in the drop-in, then `restart` |
| JSON log lines | `[log] format = "json"` in `hoster.toml` (or `HOSTER_LOG__FORMAT=json`), then `journalctl --user -u hoster -o cat \| jq` |
| Only the access log | `journalctl --user -u hoster -o cat \| grep hoster::access` |
| No access log | `Environment=RUST_LOG=info,hoster::access=off` in the drop-in |
| Check for an update | `hoster update --check`: the running and the latest version, and the release notes |
| Update | `hoster update` (`--version vX.Y.Z` for a specific release or a pre-release, `--yes` to skip the confirmation): installs the release, snapshots the database and restarts the service |
| Roll back | `hoster rollback` (`--yes` to skip the confirmation): returns to the previous installed version, undoing the database migrations it does not know; their data is lost |

**Log lines.** The proxy writes one line per request under the `hoster::access` target: host,
method, path (without the query), matched route, stack, service, generation, upstream, status,
duration in ms, response body bytes, client IP and, for a failed request, the error. Every
deployment state change is logged under `hoster::deploy` (at `warn` when it failed, was
rejected or went live with warnings), in addition to its deployment page. Deploy work runs in
spans carrying `stack`, `deployment_id`, `generation` and `service`; JSON lines list them under
`spans`.

**Stopping.** On SIGTERM hoster stops taking new work, lets in-flight deploys reach a safe
point, drains the proxy for `proxy.grace_period` (default 10 s), waits up to
`proxy.graceful_shutdown_timeout` (default 5 s) for it to stop, and exits 0. The unit's
`TimeoutStopSec=45s` must stay above the sum of the two; if you raise them, raise
`TimeoutStopSec=` in the drop-in too. App containers belong to podman and keep running while
hoster is stopped, but nothing routes to them until it is back (ADR-0034).

**Restarts.** `Restart=always` restarts hoster 2 s after it exits, whatever the reason. At boot,
`Requires=` and `After=podman.socket` make the podman socket listen before hoster starts. If
podman still cannot be reached, hoster fails startup with a message naming the socket, and
systemd tries again.

**After a reboot**, check that the service came up without a login:

```sh
sudo systemctl --user -M hoster@ status hoster
```

## Accepted risks

The update design accepts these risks (ADR-0052):

- **Tampered releases.** Integrity rests on GitHub and TLS. `SHA256SUMS` comes from the same
  release as the binary, so it catches a corrupted download, not a tampered release. There are
  no signatures (minisign, Sigstore) yet.
- **Persistence.** The service user owns and can rewrite its own binary (that is what makes
  updating from the UI possible), so a compromise of hoster can persist across restarts.
- **A bad release causes downtime** until you run `hoster rollback` over SSH; nothing reverts
  automatically.
- **Rollback loses data** that only the newer schema can hold.
- **Every update restarts hoster**, with the brief ingress gap of ADR-0034. hoster never updates
  in the background, so this only happens when you ask for it.

## Hardening

A user unit is started by the unprivileged per-user systemd instance, so it only gets the
sandboxing that instance can set up without privileges, inside a user namespace (ADR-0023). The
unit applies:

| Directives | Effect |
| --- | --- |
| `NoNewPrivileges=yes` | No setuid or file-capability escalation. |
| `PrivateUsers=yes` | A user namespace that maps only hoster's uid and gid. |
| `ProtectSystem=strict`, `ProtectHome=read-only`, `ReadWritePaths=%h/.local/share/hoster %h/.config/hoster %h/.local/lib/hoster` | The whole file system is read-only except the state directory, the configuration directory (where hoster generates its secret-store key, ADR-0029) and the binary layout (which `hoster update` writes, ADR-0052). Credential files stay readable. |
| `PrivateTmp=yes`, `PrivateIPC=yes` | Private `/tmp` and `/var/tmp`, its own SysV IPC namespace. |
| `ProtectKernelTunables=`, `ProtectControlGroups=`, `ProtectHostname=` | No writes to `/proc/sys` or cgroupfs; no hostname changes. |
| `RestrictNamespaces=yes`, `RestrictRealtime=yes`, `RestrictSUIDSGID=yes`, `LockPersonality=yes`, `MemoryDenyWriteExecute=yes` | Seccomp-based restrictions, open to unprivileged units because of `NoNewPrivileges=`. |
| `RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6` | Only Unix sockets (podman API, admin socket, sd_notify) and IP sockets. |
| `SystemCallArchitectures=native`, `SystemCallFilter=@system-service ~@privileged` | A system-call allow list; `~@privileged` also blocks module loading, clock changes and raw I/O. |
| `KeyringMode=private`, `UMask=0077` | Its own kernel keyring; new files are private to hoster. |

**Ubuntu ≥ 23.10 (`kernel.apparmor_restrict_unprivileged_userns = 1`).** AppArmor lets the user
manager create the user namespace but gives it no capabilities there. Two consequences:

- Directives that drop capabilities (`CapabilityBoundingSet=`, `PrivateDevices=`,
  `ProtectKernelModules=`, `ProtectKernelLogs=`, `ProtectClock=`) make the unit fail to start
  (`218/CAPABILITIES`), so the unit doesn't use them. hoster has no capabilities on the host, and
  `NoNewPrivileges=` and `~@privileged` cover what they added.
- systemd silently skips the namespace-based directives: `ProtectSystem=`, `ProtectHome=`,
  `ReadWritePaths=`, `PrivateTmp=`, `PrivateIPC=`, `ProtectKernelTunables=`,
  `ProtectControlGroups=` and `ProtectHostname=` do **not** apply, and hoster can write wherever
  the `hoster` user can. `NoNewPrivileges=` and the seccomp-based directives still apply.
  `hoster doctor` reports this as `unit_sandbox`. How to restore the sandbox there is open
  (OQ-36).

hoster talks to podman only through its API socket. The containers run in podman's own cgroups
and user namespaces, so none of this restricts the apps.

**Not available, or not useful, in a user unit:**

- `User=`, `DynamicUser=`, `SupplementaryGroups=`: the user instance already runs as `hoster` and
  cannot switch users.
- `AmbientCapabilities=CAP_NET_BIND_SERVICE`: the user instance has no capabilities to give
  away. Low ports come from the sysctl instead.
- `PrivateNetwork=`: hoster has to serve :80/:443 publicly and reach the registry and the ACME
  server.
- `IPAddressAllow=`/`IPAddressDeny=`, `SocketBindAllow=`/`SocketBindDeny=`, `DeviceAllow=`:
  these attach BPF programs to the unit's cgroup, which an unprivileged user instance cannot do.
- `ProtectProc=`/`ProcSubset=`: these need a fresh procfs mount, which a user unit does not get;
  `/proc` stays the host's (on systemd 262, other users' processes stay visible with
  `ProtectProc=invisible`).
- `RemoveIPC=`: has no effect on per-user services.
- `SystemCallFilter=~@resources`: left out on purpose, so the runtime may adjust its own limits.

`systemd-analyze --user security hoster.service` rates the unit **1.4 (OK)** on systemd 262. The
remaining exposure is the list above plus network access, which hoster needs.

`scripts/check-systemd-units.sh` runs `systemd-analyze --user verify` on the unit, checks the
sysctl drop-in, and checks that `TimeoutStopSec=` is above the default proxy shutdown time. CI
runs it; with `--security` it also prints the security report.
