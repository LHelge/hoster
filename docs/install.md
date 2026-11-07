# Installing hoster

hoster is one statically linked binary that runs as a systemd user service (ADR-0001, ADR-0023).
This guide sets a host up from scratch:

1. [Get the binary](#get-the-binary) (or [build it yourself](#build-it-yourself)).
2. [Prepare the host](#prepare-the-host): the `hoster` user, linger, the low-port sysctl and the
   user's `podman.socket`.
3. [Install the unit and the configuration](#install-the-unit-and-the-configuration).
4. [Check with `hoster doctor` and start](#check-and-start).

[Operating hoster](#operating-hoster) covers logs, stopping and upgrades, and
[Hardening](#hardening) lists the sandboxing the unit applies and what a user unit cannot do.

Commands prefixed with `sudo` run as an administrator. Everything else runs **as the `hoster`
user**, in a real session of that user (see [A shell as hoster](#a-shell-as-hoster)).

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

## Prepare the host

You need systemd, rootless podman (with `newuidmap`/`newgidmap` and `pasta` or `slirp4netns`,
as your distribution packages it) and cgroup v2. The unit's sandboxing uses unprivileged user
namespaces, which rootless podman needs anyway.

### The hoster user

```sh
sudo useradd --create-home --shell /bin/bash hoster
grep hoster /etc/subuid /etc/subgid
```

`useradd` normally assigns the subordinate uid/gid ranges that rootless podman needs. If `grep`
prints nothing, add them:

```sh
sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 hoster
```

hoster runs as this user only. Do not run it, or `hoster doctor`, as root.

### Linger

Linger starts the user's systemd instance at boot, so `podman.socket` and `hoster.service` run
without anybody logging in (ADR-0023):

```sh
sudo loginctl enable-linger hoster
```

### Low ports

hoster binds :80 and :443 as an unprivileged user. Install the sysctl drop-in
`deploy/sysctl/50-hoster.conf` (also a release file) and apply it:

```sh
sudo install -m 0644 50-hoster.conf /etc/sysctl.d/50-hoster.conf
sudo sysctl --system
sysctl net.ipv4.ip_unprivileged_port_start    # prints 80
```

The setting covers IPv6 binds too. It lets *every* local user bind ports 80-1023, so the host
should not run other untrusted local users (ADR-0023).

### A shell as hoster

`systemctl --user` and `journalctl --user` need the user's own session bus and
`XDG_RUNTIME_DIR`, which `sudo -u hoster` and `su hoster` do not set up. Use one of:

```sh
sudo machinectl shell hoster@     # a full session (machinectl is in systemd-container)
ssh hoster@<host>                 # if hoster has an SSH key
```

For a single command, `sudo systemctl --user -M hoster@ <command>` works too.

### The podman socket

As `hoster`:

```sh
systemctl --user enable --now podman.socket
curl -s --unix-socket "$XDG_RUNTIME_DIR/podman/podman.sock" http://d/_ping; echo   # prints OK
```

## Install the unit and the configuration

The binary goes to `/usr/local/bin/hoster` ([Get the binary](#get-the-binary)); the unit's
`ExecStart=` expects it there. Everything else lives in the hoster user's home.

As `hoster`, with `hoster.service` and `hoster.example.toml` (from `deploy/` or the release) in
the current directory:

```sh
# State: SQLite, certificates, ACME account, admin socket. It must exist, mode 0700, before the
# first start: the unit makes the file system read-only except for this directory and the
# configuration directory.
install -d -m 0700 ~/.local/share/hoster

# Configuration (ADR-0024). Set admin_host and the ACME contact at least. The directory must
# exist before the first start too; hoster generates its secret-store key file
# (~/.config/hoster/secret.key, ADR-0029) here, so back it up.
install -d -m 0700 ~/.config/hoster
install -m 0600 hoster.example.toml ~/.config/hoster/hoster.toml
"${EDITOR:-vi}" ~/.config/hoster/hoster.toml

# The user unit (ADR-0023).
install -d ~/.config/systemd/user
install -m 0644 hoster.service ~/.config/systemd/user/hoster.service
systemctl --user daemon-reload
```

If you move `state_dir` elsewhere, replace the unit's `ReadWritePaths=` in the drop-in below,
or hoster cannot write there. An empty assignment clears the list, and every path listed must
exist, or the unit fails to start (status `226/NAMESPACE`):

```ini
[Service]
ReadWritePaths=
ReadWritePaths=/srv/hoster %h/.config/hoster
```

### The admin's GitHub handle

`HOSTER_ADMIN_GITHUB` names the one GitHub account allowed into the admin UI (ADR-0021). It is
read from the environment only, so it is set with `Environment=` in the unit. The shipped unit
has an invalid placeholder, and hoster refuses to start until it is replaced. Put the real value
in a drop-in, so that installing a newer `hoster.service` does not reset it:

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

## Check and start

`hoster doctor` checks the user, the configuration, the state directory, the podman socket, the
sysctl, that :80/:443 can be bound, linger, `podman.socket`, cgroup delegation and the clock. It
reads the same environment as the service, so pass the handle in:

```sh
HOSTER_ADMIN_GITHUB=octocat hoster doctor
```

Fix every failed check; each one prints a hint. Then enable and start the service:

```sh
systemctl --user enable --now hoster
systemctl --user status hoster
```

`enable` adds it to `default.target`, which the lingering user instance reaches at boot.
`Type=notify` means the unit only becomes `active` once hoster reports it is serving.

## Operating hoster

| Task | Command |
| --- | --- |
| Status | `systemctl --user status hoster` |
| Follow the logs | `journalctl --user -u hoster -f` |
| Logs, as root | `sudo journalctl _SYSTEMD_USER_UNIT=hoster.service` |
| Stop, start, restart | `systemctl --user stop hoster` (`start`, `restart`) |
| More log detail | `Environment=RUST_LOG=hoster=debug` in the drop-in, then `restart` |

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

**Upgrades.** Install the new binary over `/usr/local/bin/hoster`, then
`systemctl --user restart hoster`. When a release changes `hoster.service`, install it as above
and run `systemctl --user daemon-reload`; your drop-in stays in place.

**After a reboot**, check that the service came up without a login:

```sh
sudo systemctl --user -M hoster@ status hoster
```

## Hardening

A user unit is started by the unprivileged per-user systemd instance, so it only gets the
sandboxing that instance can set up without privileges, inside a user namespace (ADR-0023). The
unit applies:

| Directives | Effect |
| --- | --- |
| `NoNewPrivileges=yes` | No setuid or file-capability escalation. |
| `PrivateUsers=yes`, `CapabilityBoundingSet=` | A user namespace that maps only hoster's uid and gid, with no capabilities even inside it. |
| `ProtectSystem=strict`, `ProtectHome=read-only`, `ReadWritePaths=%h/.local/share/hoster %h/.config/hoster` | The whole file system is read-only except the state directory and the configuration directory (where hoster generates its secret-store key, ADR-0029). Credential files stay readable. |
| `PrivateTmp=yes`, `PrivateDevices=yes`, `PrivateIPC=yes` | Private `/tmp` and `/var/tmp`, a minimal `/dev`, its own SysV IPC namespace. |
| `ProtectKernelTunables=`, `ProtectKernelModules=`, `ProtectKernelLogs=`, `ProtectControlGroups=`, `ProtectClock=`, `ProtectHostname=` | No writes to `/proc/sys` or cgroupfs; no module loading, kernel log, clock or hostname changes. |
| `RestrictNamespaces=yes`, `RestrictRealtime=yes`, `RestrictSUIDSGID=yes`, `LockPersonality=yes`, `MemoryDenyWriteExecute=yes` | Seccomp-based restrictions, open to unprivileged units because of `NoNewPrivileges=`. |
| `RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6` | Only Unix sockets (podman API, admin socket, sd_notify) and IP sockets. |
| `SystemCallArchitectures=native`, `SystemCallFilter=@system-service ~@privileged` | A system-call allow list. |
| `KeyringMode=private`, `UMask=0077` | Its own kernel keyring; new files are private to hoster. |

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
