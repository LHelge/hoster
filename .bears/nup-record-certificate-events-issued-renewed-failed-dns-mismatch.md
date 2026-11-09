---
id: nup
title: "Record certificate events: issued, renewed, failed, DNS mismatch"
status: open
priority: P2
created: "2026-11-09T14:05:07.914414992Z"
updated: "2026-11-09T14:05:07.914414992Z"
tags:
  - acme
  - observability
depends_on:
  - nbj
parent: "4qj"
---

## Summary
Write the certificate events of ADR-0055 §2 from the `AcmeEvent`s that the orchestrator already receives.

## Acceptance Criteria
- [ ] Issued (first certificate for a host) and renewed (a host that already had one) are separate kinds, with `not_after` in meta
- [ ] Failed carries the error and the next retry time. Repeated failures under backoff (5 min → 24 h) each write one event; that's at most one per attempt
- [ ] DNS mismatch is written when it first appears for a host or the observed records change, not on every check
- [ ] `stack` is set to the stack that owns the host's route, when there is one; `subject` links to the certificate page

## Implementation Notes
- `Orchestrator::run_acme_events` (spawned in `src/supervisor.rs` ~line 759) receives `AcmeEvent` (`src/acme/task.rs:61`). Write there through `EventLog`. The ACME task stays free of store access
- Issued vs renewed: the ACME task knows if it replaced an existing cert; add that to `AcmeEvent::Issued` if it isn't already derivable

## Testing
- Tests with the fake issuer: first issue, renewal, failure, DNS mismatch dedupe
- `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- ADR-0055, ADR-0015, ADR-0036, ADR-0051
- architecture.md §7.3
- `src/acme/task.rs`, `src/orchestrator/certs.rs`