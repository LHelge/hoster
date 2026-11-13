---
id: "5up"
title: "OTLP: the receiver route on the proxy's :80 and the injected OTEL_* variables"
status: open
priority: P2
created: "2026-11-12T19:36:39.410069219Z"
updated: "2026-11-13T17:01:54.542808288Z"
tags:
  - observability
  - runtime
  - stack
depends_on:
  - m7x
  - h4q
parent: xv9
---

## Summary
Serve the receiver where containers reach it, and point every service's SDK at it (ADR-0069 §3 and §5). The proxy's `:80` listener hands a request to the receiver when its `Host` is `host.containers.internal` and its peer is one of the host's own addresses. Rewritten from ADR-0069 on 2026-11-13; the first draft left the path open.

## Acceptance Criteria
**The route**
- [ ] On `:80`, a request with `Host: host.containers.internal` from a peer that is one of the host's own addresses reaches the receiver router of `m7x`; the same `Host` from any other peer gets the 404 of an unknown host (ADR-0032). An IPv4-mapped IPv6 peer is unmapped before the comparison
- [ ] The route is matched before the redirect to HTTPS and is never served on `:443`
- [ ] The set of the host's own addresses is read at start and kept current when interfaces change, or read in a way that cannot go stale; loopback addresses count
- [ ] `host.containers.internal` is reserved: applying a stack file that routes it is rejected with a message that says why
- [ ] Receiver requests write no request rows and no traffic buckets; they count against the connection caps of ADR-0065 like any connection from that address

**The variables**
- [ ] Every service gets `OTEL_EXPORTER_OTLP_ENDPOINT=http://host.containers.internal`, `OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf`, `OTEL_METRICS_EXPORTER=none` and `OTEL_SERVICE_NAME=<service>` in its environment as `PORT` is (`Service::container_env`), so they are in the config hash; a name the stack file sets as `env` or as a secret is left alone
- [ ] `OTEL_EXPORTER_OTLP_HEADERS=Authorization=Bearer%20<token>` (token from `j6z`) and `OTEL_RESOURCE_ATTRIBUTES` with the stack and the deployment generation are added at container create and are not hashed; the same override rule applies
- [ ] A test shows the hash of an existing service changes exactly once with this task, and does not change between two deployments of the same spec
- [ ] The UI's environment view shows the injected variables as injected, with the token masked like a secret (ADR-0029)
- [ ] The release notes entry for this epic says that the first deployment of each stack after the upgrade replaces all its services

**Doctor and docs**
- [ ] `hoster doctor` checks that a container on a stack network resolves `host.containers.internal` to `169.254.1.2` and reaches the receiver, and names podman and passt versions when it does not
- [ ] `site/content/wiki/stack-format.md` lists the injected variables beside `PORT`, and that `host.containers.internal` cannot be routed

**Still to show (ADR-0069, Consequences)**
- [ ] On the real proxy: the route is matched before the redirect. If Pingora cannot do it, stop and raise an open question; do not fall back to a dedicated port without the owner
- [ ] From a second machine: a request to the host's public address with `Host: host.containers.internal` gets 404, and a connection that claims one of the host's addresses as its source is not answered. Record the result in `docs/spikes/container-to-host-listener.md`

## Implementation Notes
- Spike `h4q` ran against a stand-in listener, on high ports; port 80 itself and Pingora are untested
- `PORT` injection (ADR-0051) in `src/stack/mod.rs` (`container_env`) and `src/orchestrator/plan.rs` (`ConfigHash`) is the pattern for the hashed variables; secrets added at create in `src/runtime/podman.rs` are the pattern for the unhashed two
- How the admin host is handed to the web app (ADR-0031) is the pattern for handing the route to the receiver

## Testing
- Proxy tests: the route for a local peer, 404 for another peer, an IPv4-mapped peer, no redirect, not on `:443`
- Stack tests: the injected variables, a stack file overriding each, the reserved host; a config-hash test
- A doctor test; an e2e test from a container if the e2e gate allows
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

## References
- ADR-0069 (§3, §5), `docs/spikes/container-to-host-listener.md`, ADR-0029, ADR-0031, ADR-0032, ADR-0051, ADR-0065
- `src/proxy/router.rs`, `src/stack/mod.rs`, `src/orchestrator/plan.rs`, `src/orchestrator/engine.rs`, `src/runtime/podman.rs`, `src/doctor/`