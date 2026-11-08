---
id: tsm
title: Startup and periodic reconciliation
status: done
priority: P1
created: "2026-11-07T14:45:17.211279069Z"
updated: "2026-11-08T00:41:25.509272570Z"
tags:
  - orchestrator
depends_on:
  - "8w4"
parent: ame
attempts: 1
---

## Summary
Reconcile persisted state with podman at startup (architecture §4 step 5, §5.4) and periodically restart exited containers per ADR-0028 (podman restart policy `no`; every ~15 s; backoff 1 s → 5 min; CrashLooping after 5 restarts in 10 min; skip stacks with an active deployment).

## Acceptance Criteria
- [ ] Startup: list `hoster.managed` containers; adopt current generation; start stopped ones; remove containers of aborted/old generations; resolve interrupted deployments per architecture §5.4 table
- [ ] Periodic loop (e.g. 15 s): restart exited containers of the current generation with exponential backoff; mark CrashLooping after threshold
- [ ] Orphan networks/containers of removed stacks cleaned up
- [ ] All actions logged with stack/service/generation

## Edge Cases & Considerations
- Container present but label missing/mismatched → leave untouched and warn (not ours)

## Testing
- FakeRuntime scenarios for each row of the §5.4 table; crash-loop backoff timing with tokio test time
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- docs/architecture.md §5.4
- ADR-0028