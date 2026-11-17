---
id: dmr
title: Preserve manual image checks across a pending reschedule
status: open
priority: P2
created: "2026-11-17T23:46:05.564911657Z"
updated: "2026-11-17T23:46:05.564911657Z"
---

## Summary
A reschedule fills the one-slot wake channel and causes a manual check to be discarded.

## Acceptance criteria
Test both command orders while the worker is busy using production queue capacity. The manual check runs when idle and subsequent scheduling uses current settings.

## References
- src/registry/updates.rs
- src/registry/updates_tests.rs
- ADR-0061