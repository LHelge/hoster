---
id: "68p"
title: Edge-triggered digest poller
status: open
priority: P2
created: "2026-11-07T14:45:17.344348469Z"
updated: "2026-11-07T21:50:54.259485521Z"
tags:
  - registry
  - orchestrator
depends_on:
  - qq8
  - mzr
  - "25t"
  - hft
parent: hxh
---

## Summary
Background task that polls tracked tags and calls `orchestrator.deploy` only on observed digest changes (ADR-0043 as amended by ADR-0051): every 5 min with jitter.

## Acceptance Criteria
- [ ] Tracks services with `track`; interval from config with jitter; per-image backoff on errors/429
- [ ] Persisted last-observed digest per (repo, tag)
- [ ] First observation: **deploy if the service has never run**, otherwise record only (ADR-0051 #1)
- [ ] Afterwards deploys only when the observed digest changes and differs from running; `source = poller`
- [ ] Does not undo manual rollbacks (a rolled-back service has run, so first-sighting logic never applies to it)

## Testing
- Unit tests with fake registry client and paused time: first sighting on never-run service → deploy; first sighting on running service → record only; change → deploy; no change → nothing; rollback not undone; webhook already deployed → noop
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- ADR-0043, ADR-0051
- ADR-0005