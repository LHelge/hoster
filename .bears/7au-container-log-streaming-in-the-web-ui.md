---
id: "7au"
title: Container log streaming in the web UI
status: open
priority: P2
created: "2026-11-07T14:45:17.369479443Z"
updated: "2026-11-07T14:45:17.369479443Z"
tags:
  - observability
  - web
depends_on:
  - ej8
  - tuv
parent: "6gn"
---

## Summary
Tail and follow container logs through bollard `logs` per ADR-0044 (k8s-file driver with max size, SSE streaming via the HTMX SSE extension).

## Acceptance Criteria
- [ ] Log view per container: tail N, since, follow via SSE/chunked stream; stdout/stderr distinguished
- [ ] Podman log size limit set at container create
- [ ] Stream closed when the client disconnects

## Testing
- Gated integration test streaming logs from a test container
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- ADR-0044