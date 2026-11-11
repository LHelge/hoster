---
id: p9n
title: Opt-in auto-update
status: open
priority: P2
created: "2026-11-09T17:23:50.069938870Z"
updated: "2026-11-09T17:23:50.069938870Z"
tags:
  - orchestrator
  - docs
depends_on:
  - j6w
parent: "6z3"
---

## Summary
Apply candidates that match a service's `auto_update` policy automatically: one revision and one deployment per stack per check pass, through the same path as the one-click update.

## Acceptance Criteria
- [ ] After each checker pass, the stack's policy-matching candidates (respecting version groups: a group moves only if every member has a matching target) become one new revision via the `update_preview` / apply path. That revision is deployed with a new `DeploymentSource::AutoUpdate`. Adding the store enum value needs a reversible migration if sources are stored as a constrained type.
- [ ] Edge-triggered, per the ADR: a (service, target digest) whose deployment failed or was rolled back is not attempted again until the candidate changes. A rollback after an auto-update holds.
- [ ] Respects the deploy queue (ADR-0040). Never runs while a self-update is in progress (ADR-0052). Skips a stack with an unfinished deployment.
- [ ] Deployment history and the stack overview label auto-updates. The dashboard shows the last auto-update per stack. If the Notifications epic (c5s) exists by then, emit an event it can use; otherwise leave a note on c5s.
- [ ] Docs: stack-format.md §2.2 (policy values with examples) and the third-party guide section on choosing a policy (risks of `minor`, why databases should usually stay `off` or `patch`).

## Testing
- Orchestrator tests with a fake checker / registry: patch applied, minor blocked by a `patch` policy, a group held back by a missing member, a failed target not retried, rollback respected, a queue interaction.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## References
- ADR-0061; ADR-0040, ADR-0041, ADR-0043, ADR-0049, ADR-0052
- `src/orchestrator/mod.rs`, `src/registry/`, `src/store/types.rs` (`DeploymentSource`)