# ADR-0064: Accept that the user unit has no file-system sandbox on Ubuntu; the hoster user is the boundary

- Status: Accepted (resolves OQ-36; amends ADR-0023)
- Date: 2026-11-11

## Context

hoster runs as a systemd user unit (ADR-0023). On Ubuntu ≥ 23.10,
`kernel.apparmor_restrict_unprivileged_userns = 1` gives the user manager a user namespace
without capabilities, so it cannot set up a mount namespace. The unit's `ProtectSystem=`,
`ProtectHome=`, `ReadWritePaths=`, `PrivateTmp=`, `PrivateIPC=`, `ProtectKernelTunables=`,
`ProtectControlGroups=` and `ProtectHostname=` are silently skipped there. `NoNewPrivileges=` and
the seccomp-based directives still apply (ADR-0023 correction, 2026-11-08). Ubuntu is the most
likely VPS image.

What that sandbox can protect is limited by a fact OQ-36 did not weigh. hoster holds the
rootless podman socket, and through the API it can start a container that bind-mounts any path
the `hoster` user can reach. podman's service runs outside hoster's mount namespace. So no unit
sandbox, under a user or a system manager, contains a fully compromised hoster. Against that, the
security boundary is the `hoster` user and its Unix permissions. The sandbox would still contain
a narrower class of fault: a bug in hoster's own process that writes or reads a wrong path.

## Decision

- **Accept the gap on Ubuntu.** Keep the user unit and its directives unchanged. Where systemd
  can apply them (Debian, Fedora, Ubuntu without the restriction), they apply. Where it cannot,
  hoster runs with `NoNewPrivileges=` and seccomp only.
- **No host-wide relaxation.** install.sh does not set `kernel.apparmor_restrict_unprivileged_userns
  = 0`, and hoster does not move to a system service for this.
- **The `hoster` user is the boundary** and is documented as such: a dedicated user with nothing
  else in its home, and no other untrusted local users on the host (as ADR-0023 already asks for
  low ports).
- `hoster doctor`'s `unit_sandbox` check stays **advisory**, and it and the CI unit test keep
  reporting the condition, now pointing at this ADR.

## Alternatives considered

- **Lift the restriction host-wide** (`kernel.apparmor_restrict_unprivileged_userns = 0` in
  `50-hoster.conf`). It would restore the sandbox for one line, against wrong-path bugs only. It
  undoes an Ubuntu hardening default for every local user. Not chosen.
- **A system service with `User=hoster`** and `CAP_NET_BIND_SERVICE`. It would give a root-built
  sandbox and a root-owned unit, but it needs ordering across systemd instances, sudo or polkit
  for the CLI's restarts, and a migration of existing installs. Because of the podman socket, it
  gains little against a real compromise. Not chosen. ADR-0023's original rejection stands.
- **An AppArmor profile granting `userns`.** On `systemd-executor` it applies to every user unit,
  so it is the host-wide relaxation with more moving parts. On the hoster binary it is too late,
  since systemd creates the namespace before `exec`. Rejected.

## Consequences

- On Ubuntu ≥ 23.10, a hoster bug that writes to a wrong path can reach anything the `hoster`
  user can write. That includes its unit file, its shell profile and, by design (ADR-0052), its
  own binaries.
- Nothing changes for existing installs.
- If a later need (for example ending the low-ports-for-every-user sysctl) brings back the system
  service, the sandbox comes with it. That would be a new ADR.
