# ADR-0071: Stack networks take their subnets from one configured pool

- Status: Accepted (resolves OQ-46; amends ADR-0006 and ADR-0024)
- Date: 2026-11-14

## Context

Each stack has its own podman network, `hoster-<stack>` (ADR-0006), and podman picks its subnet
from its default pools. hoster's proxied connections reach a routed container through the
loopback port it publishes (ADR-0007), and rootless podman forwards them into the network, so
the container sees them come from an address inside its own stack's subnet. An app that trusts
forwarded headers only from its proxy (nginx `set_real_ip_from`, a framework's trusted
proxies) cannot name that range in advance and ends up trusting all of `10.0.0.0/8` (user
feedback, 2026-11-14).

## Decision

- **One pool.** hoster.toml's `network_pool` (`HOSTER_NETWORK_POOL`), default `10.89.0.0/16`,
  is the range every stack network is created in. It is checked at startup: an IPv4 range in
  RFC 1918 private space, no smaller than a /24.
- **A /24 per stack.** When hoster creates a stack network it lists podman's networks and passes
  the lowest /24 of the pool that overlaps none of them as the network's subnet. podman keeps the
  subnet; hoster stores nothing. A full pool fails the deploy with an error naming
  `network_pool`.
- **No stack-file setting.** Stack files do not name subnets.
- **Existing networks are left alone.** A network created before this change, or before the pool
  was changed, keeps its subnet until hoster creates it again (after the stack is removed and
  applied again). `hoster doctor` reports a stack network outside the pool, as advice.
- **Documented** for apps: trust `network_pool` (by default `10.89.0.0/16`) as the proxy's
  range. The pool appears in Settings' effective configuration.

## Alternatives considered

- **Per-stack `[network] subnet`.** Exact per stack, but a new stack-file section, an overlap
  check at apply against every other stack and podman's own networks, and a rule for what
  changing it does to a running stack (the network must be recreated, so every container
  restarts). Other stacks sharing the pool gain nothing from it: ADR-0046 already keeps stacks
  from reaching each other.
- **A deterministic /24 per stack** (from its name or id). Predictable, but collisions with
  each other and with non-hoster networks need the same free-subnet search anyway.
- **Documenting the gateway only.** No code, but the address stays podman's choice and moves
  when the network is recreated.
- **Recreating existing networks to move them into the pool.** Every container in the stack
  would restart; not worth an unasked-for outage.
- **No default (opt-in pool).** Unchanged behaviour, but the documented range would hold only
  for installs that set it. The default matches podman's own first default pool, so most
  existing stack networks already sit inside it.

## Consequences

- Apps can trust one documented range instead of `10.0.0.0/8`.
- Up to 256 stacks per /16, far more than one VPS runs.
- A non-hoster podman network of the hoster user inside the pool takes its /24 out of the
  allocation; that user is dedicated to hoster (ADR-0023), so this is rare.
- The trusted range includes every stack's network, not only the app's own. Other stacks cannot
  reach the app (ADR-0046), so they cannot use that trust.
