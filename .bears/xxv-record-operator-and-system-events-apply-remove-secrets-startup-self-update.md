---
id: xxv
title: "Record operator and system events: apply/remove, secrets, startup, self-update"
status: done
priority: P2
created: "2026-11-09T14:05:07.931510925Z"
updated: "2026-11-09T16:17:01.322695875Z"
tags:
  - orchestrator
  - update
  - observability
depends_on:
  - nbj
parent: "4qj"
attempts: 1
---

## Summary
Write the operator and system events of ADR-0055 §2.

## Acceptance Criteria
- [ ] Stack applied (revision number and hash in meta) and stack removed (whether volumes were deleted). Since `stack` has no FK, the removal event survives the stack
- [ ] Secret set / deleted: the secret's **name** and scope only, never the value
- [ ] Who did it: the operator's GitHub handle in meta for UI actions (ADR-0031)
- [ ] hoster started: the version, written once per process start
- [ ] Self-update applied (from → to version) and rolled back (`hoster rollback`)

## Implementation Notes
- Apply/remove/secrets go through the orchestrator (`src/orchestrator/mod.rs`, `stack_secrets.rs`); write there, not in the web handlers, so every entry point is covered
- Startup event: in `src/supervisor.rs` after the store opens
- Update/rollback: `src/update/`. **Rollback runs the newer binary, which runs the down-migrations** (ADR-0052). Write the rollback event *before* migrating down. If the target version predates the events table, the down-migration drops it and the event goes with it; that's acceptable (ADR-0055 consequences)
- Update applied: the new binary has no memory of the old one; write the event from the new binary's first start when the recorded previous version differs, or from the updater before it hands over. Pick whichever `src/update/` makes simpler and note it in the commit

## Testing
- Orchestrator tests for apply/remove/secret; update tests with `src/update/fake.rs`
- `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- ADR-0055, ADR-0029, ADR-0030, ADR-0031, ADR-0049, ADR-0052
- `src/orchestrator/`, `src/update/`, `src/supervisor.rs`