# ADR-0034: Accept a short ingress gap when hoster restarts; spike socket activation later

- Status: Accepted (resolves OQ-18)
- Date: 2026-11-07

## Decision

- **v1 accepts the gap.** Restarting or upgrading hoster makes ingress unavailable for about
  1–2 s. Containers keep running.
- **Startup is kept fast** to shorten the gap:
  - certificates and routes load from disk and SQLite
  - running containers are adopted rather than restarted
  - Pingora starts with a populated RouteTable
- **Follow-up spike: systemd socket activation.** A `hoster.socket` user unit with
  `ListenStream=80`/`443`, which works with the sysctl from ADR-0023, would keep the listening
  sockets alive across restarts. Clients would then queue in the kernel backlog instead of being
  refused. The open question is whether Pingora can adopt inherited fds; its own upgrade path
  already does this internally.

## Alternatives considered

- **Pingora graceful upgrade (fd handover between two processes) in v1**: needs two processes at
  once, systemd cooperation and a handover of orchestrator state. Too complex for now.
- **No follow-up**: socket activation is cheap enough to be worth investigating.

## Consequences

- In-flight requests are cut during a restart. Upgrades should be scheduled.
