# ADR-0023: Run hoster as a systemd user service with linger and a lowered unprivileged-port start

- Status: Accepted (resolves OQ-09; binary location amended by ADR-0052)
- Date: 2026-11-07

## Context

The rootless podman socket (`/run/user/<uid>/podman/podman.sock`) is socket-activated by the
**user** unit `podman.socket`, so it needs the `hoster` user's systemd instance, and that needs
`loginctl enable-linger hoster`. hoster must also bind :80 and :443 as that unprivileged user.

## Decision

- **Users.** A dedicated `hoster` user with linger enabled.
- **Units.** `hoster.service` runs as a **user unit** in that user's systemd instance:
  - `~hoster/.config/systemd/user/hoster.service`
  - `Requires=podman.socket` and `After=podman.socket`
  - `Type=notify`, `Restart=always`, `WantedBy=default.target`
- **Low ports.** `/etc/sysctl.d/50-hoster.conf` sets `net.ipv4.ip_unprivileged_port_start=80`.
  This also applies to IPv6 binds.
- **Paths:**
  - state in `~hoster/.local/share/hoster` (`$XDG_DATA_HOME/hoster`)
  - config in `~hoster/.config/hoster` (ADR-0024)
- **Logs.** In the user journal: `journalctl --user -u hoster` as `hoster`, or
  `journalctl _SYSTEMD_USER_UNIT=hoster.service` as root.

## Alternatives considered

- **System service with `User=hoster` and `AmbientCapabilities=CAP_NET_BIND_SERVICE`.** The
  capability would apply to hoster only, and system units get full sandboxing. It was rejected as
  more complex: it has to be ordered against `user@<uid>.service` across systemd instances, and
  hoster has to wait for the podman socket to appear.
- **The system `podman.socket`** (`/run/podman/podman.sock`). That is rootful podman, which
  contradicts ADR-0002. Rejected.
- **Rootless `podman system service` started from a system unit with `User=`.** Rejected:
  - The containers' conmon processes land in the service's cgroup, so stopping the unit kills
    the apps unless `KillMode`/`Delegate` workarounds are added.
  - There is no user session, so the systemd cgroup manager is unavailable and `cgroupfs` is
    needed instead.
  - It goes against podman's own guidance for rootless setups.

## Consequences

- **Low ports are open to every local user.** Any local process can bind 80–1023. This is
  acceptable on a single-purpose VPS. The residual risk is another local process taking :80/:443
  while hoster restarts. A firewall does not mitigate this (it is a local-bind concern), so the
  host should not run other untrusted local users.
- **Apps cannot use it.** Apps never get this ability, because only hoster decides what podman
  publishes, and only on `127.0.0.1` (ADR-0007).
- **Weaker sandboxing.** User units cannot use every sandboxing directive. Hardening is
  best-effort (`NoNewPrivileges`, `PrivateTmp`, `ProtectSystem` where supported unprivileged).
- **Encrypted credentials need checking.** systemd ≥ 256 supports user-scoped encrypted
  credentials (`systemd-creds --user`, `LoadCredentialEncrypted=` in user units). Whether the
  target distro's systemd supports this decides the key source for ADR-0029, and must be verified.
- **Restarts.** The unit restarts hoster automatically. Container restarts after a reboot are
  ADR-0028.
- **Doctor checks.** `hoster doctor` checks linger, the sysctl value, `podman.socket` and the
  delegated cgroup controllers.
