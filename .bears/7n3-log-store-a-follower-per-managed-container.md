---
id: "7n3"
title: "Log store: a follower per managed container"
status: open
priority: P2
created: "2026-11-12T19:33:33.372208975Z"
updated: "2026-11-12T19:33:33.372208975Z"
tags:
  - observability
  - runtime
  - orchestrator
depends_on:
  - azv
  - r8w
parent: f8z
---

## Summary
ADR-0067 §1: for every running managed container hoster reads stdout and stderr through bollard `logs` with follow and timestamps, from the moment it starts the container until it is removed, parses each line and hands the record to the recorder. This is the hard part of the epic: deploys, restarts, crash loops and removals must give neither duplicates nor gaps. Resuming after a hoster restart is the next task.

## Acceptance Criteria
- [ ] A follower starts when hoster starts a managed container (deploy, reconciler restart, rollback, start-first and stop-first alike) and before its health checks, so start-up output is captured
- [ ] It ends when the container is removed; a container that exits and is restarted by the reconciler (ADR-0028) is a new container and gets a new follower
- [ ] Each record carries stack, service, generation and stream from what hoster knows about the container (labels), never from the line
- [ ] A record's time is podman's timestamp of the line (ADR-0067 §3); the follower updates `log_cursors` with the last line handed to the recorder, in the recorder's commit
- [ ] A per-container line-rate cap (a constant, set here): the excess is dropped and one marker record per gap carries the number of dropped lines (ADR-0067 §7)
- [ ] One supervisor-owned registry of followers keyed by container id; stopping hoster stops them all before the recorder
- [ ] An unmanaged container (not started by hoster) is never followed
- [ ] A follower whose stream ends unexpectedly while the container still runs reconnects from its cursor, with backoff

## Implementation Notes
- `PodmanRuntime::logs` and `ContainerLog` in `src/runtime/podman.rs` already decode the multiplexed frames into lines with timestamps; extend the runtime trait if needed, and give `src/runtime/fake.rs` the same behaviour for tests
- Where hoster starts and removes containers: `src/orchestrator/engine.rs` (deploys), `reconcile.rs` and `exits.rs` (restarts), `gc.rs` (removal). Hook the follower registry there through the orchestrator, not from the web layer
- Bytes to `str` lossily (the parser expects `&str`); lines split at `\n`

## Edge Cases
- A container that exits within milliseconds of starting (crash loop): its lines are still read until the stream ends
- Two generations of a service running at once during a start-first deploy: both followed, told apart by generation
- podman restarts under hoster: streams end, followers reconnect from their cursors

## Testing
- Tests with the fake runtime: start, exit and restart, removal, crash loop, rate cap marker, labels to record fields
- An e2e test in `src/e2e_tests.rs` (behind its existing gate) with a real container printing JSON and text
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

## References
- ADR-0067 (§1, §3, §7), ADR-0044, ADR-0028, ADR-0017
- `src/runtime/podman.rs`, `src/runtime/fake.rs`, `src/orchestrator/`, `src/supervisor.rs`