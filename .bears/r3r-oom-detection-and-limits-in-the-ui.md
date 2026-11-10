---
id: r3r
title: OOM detection and limits in the UI
status: done
priority: P2
created: "2026-11-09T17:23:25.599826110Z"
updated: "2026-11-10T21:58:12.149479532Z"
tags:
  - orchestrator
  - web
  - observability
depends_on:
  - xxb
parent: "9q5"
attempts: 1
---

## Summary
Implement ADR-0059 §6: name OOM kills from podman's sticky `State.OOMKilled` flag in restart events, restart records and deploy aborts. Show each service's limits and runtime options in the UI.

## Acceptance Criteria
- [x] **Flag tracking.** `OOMKilled` stays true across `start`, so hoster keeps the flag it last saw per container. It updates the flag at create (false), before each start it does, and when it first sees a container running (adoption at startup included).
- [x] **Exit reason** (one type with `Display`), for an exit hoster did not cause: before a reconciler restart (ADR-0028), or while a deploy probes the container:
  - exit 137 with the flag true reads "OOM-killed (limit 512MiB)", since an unsent SIGKILL is almost always the OOM killer;
  - any other exit where the flag turned true since hoster last saw it reads "exited <code> after an OOM kill (limit 512MiB)";
  - "(no memory limit)" replaces the limit when none is set, and the limit uses `ByteSize`'s `Display`;
  - an exit with no OOM reads as today (`exit <code>`).
- [x] **Restart event** (`ServiceEvents::restarted`, `src/orchestrator/event_log.rs`): the message carries the exit reason, replacing today's ", OOM killed" suffix from the flag alone. `meta` is unchanged. This narrows ADR-0055's "OOM if podman reports it" to these rules.
- [x] **Restart record** (`Restarts` / `RestartStats`, `src/orchestrator/reconcile.rs`): keeps the last exit reason (amends ADR-0028). It is shown in the crash-loop detail on the stack overview and in the dashboard warnings. A failed restart (e.g. a controller undelegated after apply: HTTP 500, container left `created`) records the start error as the reason.
- [x] **Deploy abort:** a container that exits while `HealthChecking` or `Replacing` probes it aborts the deploy with the exit reason in the deployment's failure reason and `failed` event, e.g. "app exited: OOM-killed (limit 512MiB)" (amends ADR-0038). (An exit without a nameable OOM keeps today's "app: container … exited (1) before it became healthy".)
- [x] **Limits in the UI:** the stack overview's services card (`src/web/stacks/overview/services.rs`) shows each service's `limits` and runtime options (`shm_size`, `tmpfs`, `init`) from the committed revision, sizes in `ByteSize`'s `Display`.
- [x] If epic qmw (resource usage meters) has landed, its memory meter shows usage against the limit; otherwise leave a note on qmw. (qmw has not landed: note added to its body.)

## Implementation Notes
- The tracked flag lives in memory with the reconciler's per-container state. It is lost on a hoster restart, and "first seen running" covers that case. Read the flag from the inspect the reconciler already does before a restart (`ContainerInfo.oom_killed`).
- Exits hoster causes (its own stops during `Replacing`, cleanup, stack stop) are never classified.
- The reason needs the service's `memory` limit from the revision the container was created from (the committed revision for the reconciler, the target for a deploy).
- The fake runtime (`src/runtime/fake.rs`) resets `oom_killed` on start (`set_state`), unlike podman. Make it sticky for the container's lifetime (architecture §1.8, OOM), and let a test set the flag without an exit, to model an OOM-killed child.

## Edge Cases
- An OOM kill of a child in a running container shows only when the container exits (no podman `oom` event; ADR-0059 Consequences).
- After a 137 OOM exit and a restart, a later exit 1 with the flag still true and unchanged reads "exit 1", not OOM: only a flag that turned true counts.
- A 137 with the flag true is named even if the flag was already true, since the exit itself is the evidence.
- In a run hoster did not see start (adopted with the flag already true), an OOM is named only if it exits 137.

## Testing
- Reconciler tests (`reconcile_tests.rs`) with the fake runtime: 137 + flag, exit 0 after a flag change, a sticky flag with a later non-OOM exit, adoption with the flag true, no limit, a failed restart's reason in `RestartStats`.
- Event-log test for the restart message; engine abort-reason tests for `HealthChecking` and `Replacing`.
- Web rendering tests: limits on the overview, the reason in the crash-loop detail and the dashboard warnings.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## References
- ADR-0059 §6 and Consequences; ADR-0028, ADR-0038, ADR-0055
- `docs/architecture.md` §1.8 (OOM), §2.1 (Reconciler), §5.1 (`HealthChecking`, `Replacing`), §11 (Producers)
- `src/orchestrator/reconcile.rs`, `src/orchestrator/event_log.rs`, `src/orchestrator/engine.rs`, `src/runtime/mod.rs`, `src/runtime/fake.rs`, `src/web/stacks/overview/services.rs`, `src/web/dashboard.rs`
