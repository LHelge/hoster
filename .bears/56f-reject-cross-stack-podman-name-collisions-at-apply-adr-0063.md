---
id: "56f"
title: Reject cross-stack podman name collisions at apply (ADR-0063)
status: open
priority: P2
created: "2026-11-11T21:44:33.618038726Z"
updated: "2026-11-11T21:44:33.618038726Z"
tags:
  - stack
  - orchestrator
  - runtime
---

## What
ADR-0063 (resolves OQ-35): apply rejects a stack whose `<stack>-<service>` (containers) or `<stack>-<volume>` (volumes) equals another stack's, before anything is created. Services and volumes are checked separately. Kept volumes of removed stacks (podman volumes labelled `hoster.stack=<s>`) still claim `<s>-<volume>`. Re-applying a stack never collides with itself. The existing deploy-time label check on volumes stays.

## Do
- A claims type next to `RouteClaims` (src/routing/collision.rs is the model: `check(candidate, others, …)`, ignoring the candidate's own previous version), or a deep extension of it, that owns the two podman-name sets and reports a typed error naming both stacks, the kind (service/volume) and the podman name.
- Call it wherever apply/validation calls `RouteClaims::check`, feeding the other stacks' committed revisions plus the labelled volumes the runtime lists for stacks that no longer exist.
- The error is shown in the apply preview like a route collision.
- Tests: `a-b`/`c` vs `a`/`b-c` for services and for volumes (rejected); service `x` vs volume `x` (allowed); re-apply of the same stack (allowed); a removed stack's kept volume (rejected until deleted); the existing `a_volume_labelled_for_another_stack_is_never_mounted` stays green.

## Acceptance
- [ ] Apply refuses both collision kinds with an error naming the other stack and the podman name
- [ ] Kept volumes of removed stacks count
- [ ] Docs already describe it (ADR-0063, architecture §8, stack-format); adjust wording if the error differs

## References
- ADR-0063, ADR-0012, ADR-0030; `docs/architecture.md` §8
- `src/routing/collision.rs` (`RouteClaims`), `src/runtime/mod.rs` (`ServiceGeneration`, `VolumeMount::named`, `VolumeSpec::for_stack`), `src/orchestrator/tests.rs` (`a_volume_labelled_for_another_stack_is_never_mounted`)