# ADR-0001: One self-contained binary on the host under systemd

- Status: Accepted
- Date: 2026-11-07

## Context

hoster has to own ports 80/443, talk to the rootless podman socket, manage containers' lifecycle
(including surviving their restarts), and persist ACME keys and certificates. The deployment target
is a single VPS administered by one person, so operational simplicity matters more than
portability.

## Decision

hoster ships as one statically-linked-as-practical Rust binary that runs directly on the host as a
systemd service. The proxy, ACME client, deploy engine, webhook receiver and poller all live in this
one process. (It runs as a systemd user service, ADR-0023.)

## Alternatives considered

- **Run the orchestrator in a container** (the Coolify/Portainer model). Rejected:
  - It needs the podman socket mounted in, which is exactly the "container controls the host's
    container runtime" capability we deny to apps (ADR-0019); the orchestrator container would
    become the highest-value target on the box.
  - Rootless podman-in-podman or socket passthrough adds user-namespace and SELinux/label friction,
    and binding 80/443 from inside a rootless container adds another port-forwarding hop
    (`rootlessport`/pasta) in front of the proxy, hiding client IPs.
  - Upgrading the orchestrator would mean the orchestrator restarting its own container — a
    chicken-and-egg problem that a host-level systemd unit doesn't have.
  - Logs, restarts, resource accounting and boot ordering come for free from systemd/journald.
- **Several binaries** (separate proxy daemon, separate agent). Rejected: more IPC, more units, more
  version skew, no real isolation benefit on one host (see ADR-0008).

## Consequences

- Installation is "copy binary, install unit, enable". Upgrades are a binary swap plus restart.
- A crash or restart of hoster briefly takes down the proxy for all apps (containers keep running).
  Mitigations are ADR-0034.
- hoster must be careful with privileges: it runs as an unprivileged user with only
  the unprivileged-port sysctl (ADR-0023).
