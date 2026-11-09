---
id: yvr
title: Record deploy events from the orchestrator
status: open
priority: P2
created: "2026-11-09T14:05:07.877849210Z"
updated: "2026-11-09T14:05:07.877849210Z"
tags:
  - orchestrator
  - observability
depends_on:
  - nbj
parent: "4qj"
---

## Summary
Write the deploy events of ADR-0055 §2: started, succeeded, failed and rolled back, one line per outcome (not every state-machine transition; those stay in `deployment_events`).

## Acceptance Criteria
- [ ] Started: written when a deployment is accepted (not for a coalesced/no-op target, ADR-0040). Meta: source (UI / webhook / poller) and the services and short digests
- [ ] Succeeded: written in the commit-point transaction (§5.4), so a crash can't leave a committed deploy without its event. Severity `Success`, with the duration
- [ ] Failed: carries the error. A cancellation (`CANCELLED`) and "stack removed" show as `Warning`, not `Error`
- [ ] Rolled back (ADR-0041): distinguishable from a normal deploy, with the target deployment id
- [ ] The commit sha in meta when the trigger provides one (the webhook's OIDC claims); otherwise left out
- [ ] `subject` links to the deployment page

## Implementation Notes
- `src/orchestrator/engine.rs`, `machine.rs`, `mod.rs`; the `deployments.source` column already records the trigger
- Check what `src/store/webhook.rs` / the webhook handler keeps of the OIDC claims for the sha; if nothing, thread it through the deploy request

## Edge Cases
- Crash recovery (§5.4) finishing a deployment after a restart writes its outcome exactly once

## Testing
- Orchestrator tests with the fake runtime: success, failure, cancel, coalesced no-op (no event), rollback
- `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- ADR-0055, ADR-0017, ADR-0040, ADR-0041, ADR-0047
- architecture.md §5.1, §5.4
- `src/orchestrator/`