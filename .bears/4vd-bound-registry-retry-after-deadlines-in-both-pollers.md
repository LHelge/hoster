---
id: "4vd"
title: Bound registry Retry-After deadlines in both pollers
status: open
priority: P2
created: "2026-11-17T23:46:05.648673590Z"
updated: "2026-11-17T23:46:05.648673590Z"
---

## Summary
Both workers add an unrestricted registry-provided delay to an Instant. Overflow can terminate the task.

## Acceptance criteria
Exercise an unrepresentable deadline in both workers. Verify they remain operational, apply a bounded delay, and still honor ordinary retry delays.

## References
- src/registry/client.rs
- src/registry/poller.rs
- src/registry/updates.rs