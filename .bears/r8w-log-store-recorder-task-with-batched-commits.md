---
id: r8w
title: "Log store: recorder task with batched commits"
status: done
priority: P2
created: "2026-11-12T19:33:20.354361353Z"
updated: "2026-11-12T20:32:13.029831661Z"
tags:
  - store
  - observability
depends_on:
  - "7ne"
parent: f8z
attempts: 1
---

## Summary
The one writer of `logs.db` (ADR-0067 §6): a task that receives log records and request rows over a bounded channel and commits them in one transaction about once a second and on graceful shutdown. Producers (followers, the proxy, hoster's own layer) only ever `try_send`.

## Acceptance Criteria
- [ ] A recorder handle with a cheap clone for producers; `try_record(LogRecord)` and `try_request(RequestRow)` never await and never block; when the channel is full the item is dropped and a per-source drop counter goes up
- [ ] The recorder commits everything received since the last commit in one transaction about once a second (a constant), and also when a batch reaches a size cap, so a burst does not hold a huge transaction
- [ ] Committed state is announced (a `tokio::sync::watch` of the last committed record id and request id) for the follow stream of the logs UI and for tests; pages read committed data only
- [ ] On graceful shutdown the supervisor stops producers first (followers, the proxy) and then the recorder, which commits what it holds; this order is documented next to the traffic recorder's in `docs/architecture.md` §4
- [ ] A failed commit is logged (under a target excluded by ADR-0068 so it is not recorded into itself), counted, and the batch is dropped; the recorder keeps running
- [ ] Drop counters (channel full, failed commits) are readable for the System page task

## Implementation Notes
- `src/stats/recorder.rs` is the pattern: a supervisor-owned task, a timer, one transaction, a final flush when stopped
- Channel capacity and batch caps are constants at the top of the module
- Multi-row `INSERT` statements (chunked under SQLite's variable limit) rather than one statement per row

## Edge Cases
- The store is unavailable (the open failed): the recorder is not started and `try_*` drop everything with the counter still counting, so producers need no special case
- Shutdown while a commit is in flight: the final flush waits for it, under the supervisor's shutdown deadline

## Testing
- Tests with a temporary `LogStore`: batching into one transaction, the watch value after commit, drop on a full channel, the final flush
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

## References
- ADR-0067 (§5, §6), ADR-0068 (§4), ADR-0062 (§7)
- `src/stats/recorder.rs`, `src/supervisor.rs`, `docs/architecture.md` §4