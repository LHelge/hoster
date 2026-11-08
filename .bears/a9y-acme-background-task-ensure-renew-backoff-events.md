---
id: a9y
title: "ACME background task: ensure, renew, backoff, events"
status: in_progress
priority: P1
created: "2026-11-07T14:45:17.275247781Z"
updated: "2026-11-08T01:42:35.724590044Z"
tags:
  - tls
  - acme
depends_on:
  - saw
  - pav
  - hft
parent: "52q"
attempts: 2
---

## Summary
The long-running ACME task: receives `AcmeCommand::Ensure{hosts}` from the orchestrator, renews on a timer, backs off per host, and emits `AcmeEvent`s (architecture §3.3, §7.3). The DNS check is advisory (ADR-0051 #2).

## Acceptance Criteria
- [ ] Ensure issued for hosts without a valid cert (admin host included); triggered on stack apply (ADR-0036); hosts no longer routed are not renewed
- [ ] **Advisory DNS check:** hoster's public addresses = A/AAAA of the admin host resolved at startup and hourly, overridable by `acme.public_addresses` (config key added in `26p`'s `Config`). For each host, compare its A/AAAA with them; a mismatch is recorded as `DnsMismatch { host, observed, expected }` on the host's status and emitted as an event, but issuance is still attempted under the normal backoff
- [ ] Renewal tick (hourly): renew at < 1/3 lifetime remaining or within ARI window if supported
- [ ] Per-host exponential backoff 5 min → 24 h, persisted so restarts don't reset it
- [ ] Events to the orchestrator (`acme_events`), which fans them out on `ui_events` (Issued/Failed/DnsMismatch) for status

## Testing
- Unit tests with a fake ACME client trait, a fake resolver and tokio paused time: renewal timing, backoff, ensure dedupe, mismatch still issues and is reported
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- ADR-0015, ADR-0036, ADR-0051
- docs/architecture.md §7.3