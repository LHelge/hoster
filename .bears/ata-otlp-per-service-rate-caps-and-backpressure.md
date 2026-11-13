---
id: ata
title: "OTLP: per-service rate caps and backpressure"
status: open
priority: P3
created: "2026-11-12T19:36:39.399541882Z"
updated: "2026-11-12T19:36:39.399541882Z"
tags:
  - observability
depends_on:
  - m7x
parent: xv9
---

## Summary
ADR-0069 §7: no sampling, a rate cap per service. Checked against ADR-0069 on 2026-11-13. A chatty or broken exporter must not fill `logs.db` or starve the recorder for everyone else.

## Acceptance Criteria
- [ ] Token-bucket caps per (stack, service) for spans per second and log records per second (constants), applied in the handler before the recorder
- [ ] What is over the cap is dropped and reported in `partial_success`; one marker record per service and gap says how many spans or records were dropped, like the line-rate marker of ADR-0067 §7
- [ ] The drop counts appear on the System page's log store card by service
- [ ] A per-service cap on concurrent requests to the receiver; beyond it 429 with `Retry-After`, which SDKs honour

## Testing
- Unit tests for the buckets; handler tests for partial success, markers and 429
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

## References
- ADR-0069 (§7), ADR-0067 (§7)
- The receiver task `m7x`, the System page card task `uw9`