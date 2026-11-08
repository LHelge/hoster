---
id: gnz
title: "Health checker: HTTP and TCP probes"
status: in_progress
priority: P1
created: "2026-11-07T14:45:17.288083789Z"
updated: "2026-11-08T00:06:31.677455917Z"
tags:
  - orchestrator
depends_on:
  - "2zu"
  - "8w4"
  - hft
parent: mdd
attempts: 1
---

## Summary
Probe new containers from the host on their 127.0.0.1 port per ADR-0038 as amended by ADR-0051 #3. Defaults: HTTP `GET /healthz` on the first route's container port, expecting 2xx–3xx, for routed services; "running and not restarted for 5 s" for non-routed services.

## Acceptance Criteria
- [ ] `http` probe (path, port, expected status class) and `tcp` probe; interval, timeout, healthy_after, start_period
- [ ] Default for routed services without `health`: `GET /healthz` on the first route's container port (mapped to its allocated loopback port), 2xx–3xx
- [ ] Abort early if the container exits during the check (inspect)
- [ ] Probe results recorded as deployment events; a failing default probe's event text says which path was probed and that `health.http.path` can override it

## Testing
- Unit tests against local test servers that become healthy after N requests / never / crash; default-path test
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- ADR-0038, ADR-0051
- docs/stack-format.md §2.4